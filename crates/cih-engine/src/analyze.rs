use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{Edge, GraphArtifacts, JarInfo, Node, RepoMap, VersionId};
use cih_jar::JarApiExtractor;
use cih_parse::{ParseOutput, ParsedUnit};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use crate::db::{load_to_falkor, LoadOutcome};
use crate::file_cache::{
    hash_all, load_cached_parsed, save_cached_parsed, FileHashIndex, ImporterIndex,
};
use crate::scope::{self, ScopeFile, ScopeRequest};
use crate::versioning::{content_version, latest_graph_artifacts, prune_other_versions};
use crate::{scan, DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

#[derive(Debug)]
pub(crate) struct AnalyzeFlags {
    pub(crate) all: bool,
    pub(crate) modules: Vec<String>,
    pub(crate) include: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) include_decompiled: bool,
    pub(crate) scope: Option<PathBuf>,
    pub(crate) json: bool,
    pub(crate) falkor_url: Option<String>,
    pub(crate) graph_key: Option<String>,
    pub(crate) no_load: bool,
    pub(crate) no_cache: bool,
}

pub(crate) fn run_analyze(repo: PathBuf, flags: AnalyzeFlags) -> Result<()> {
    let scan = scan::scan_repo(&repo)?;
    let repo_map_path = scan::write_repo_map(&scan.repo_map)?;
    let request = build_scope_request(&repo, &flags)?;

    if !request.has_selector() {
        scan::print_summary(&scan.repo_map, &repo_map_path);
        println!();
        println!("Choose a scope: --all | --module <names> | --include <glob> | a cih.scope.toml");
        process::exit(2);
    }

    let emit = analyze_emit_with_options(
        &scan,
        request,
        AnalyzeCacheOptions {
            use_cache: !flags.no_cache,
            allow_noop: !flags.no_cache,
        },
    )?;

    let load = if emit.reused_artifacts {
        tracing::info!("No source changes detected; reusing existing artifacts and live graph");
        LoadOutcome::Reused
    } else if flags.no_load {
        tracing::info!("Skipping FalkorDB load (--no-load)");
        LoadOutcome::Skipped
    } else {
        let falkor_url = flags.falkor_url.as_deref().unwrap_or(DEFAULT_FALKOR_URL);
        let graph_key = flags.graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
        match load_to_falkor(falkor_url, graph_key, &emit.artifacts) {
            Ok(stats) => {
                tracing::info!(
                    nodes = stats.nodes,
                    edges = stats.edges,
                    url = falkor_url,
                    graph = graph_key,
                    "FalkorDB bulk load complete"
                );
                LoadOutcome::Loaded(stats)
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    url = falkor_url,
                    "FalkorDB bulk load failed — artifacts are on disk, re-run or load manually"
                );
                LoadOutcome::Failed(format!("{err:#}"))
            }
        }
    };

    if flags.json {
        println!("{}", serde_json::to_string_pretty(&emit.summary(&load))?);
    } else {
        emit.print_human(&load);
    }

    let graph_key = flags.graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
    crate::registry::persist_analyze(&emit, graph_key);

    if matches!(load, LoadOutcome::Failed(_)) {
        process::exit(3);
    }
    Ok(())
}

pub(crate) fn run_resolve(
    repo: PathBuf,
    falkor_url: Option<String>,
    graph_key: Option<String>,
    no_load: bool,
    json: bool,
) -> Result<()> {
    let scope_path = repo.join(".cih").join("scope.json");
    let scope_file: ScopeFile = {
        let raw = std::fs::read_to_string(&scope_path).with_context(|| {
            format!(
                "no saved scope at {} — run `analyze` first",
                scope_path.display()
            )
        })?;
        serde_json::from_str(&raw)
            .with_context(|| format!("malformed scope file at {}", scope_path.display()))?
    };

    let jars = load_jars_from_repo_map(&repo);
    let emit = analyze_from_scope_with_options(
        scope_file,
        scope_path,
        &jars,
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: false,
        },
    )?;

    let load = if no_load {
        tracing::info!("Skipping FalkorDB load (--no-load)");
        LoadOutcome::Skipped
    } else {
        let url = falkor_url.as_deref().unwrap_or(DEFAULT_FALKOR_URL);
        let key = graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
        match load_to_falkor(url, key, &emit.artifacts) {
            Ok(stats) => {
                tracing::info!(
                    nodes = stats.nodes,
                    edges = stats.edges,
                    "FalkorDB resolve load complete"
                );
                LoadOutcome::Loaded(stats)
            }
            Err(err) => {
                tracing::warn!(error = %err, "FalkorDB load failed after resolve");
                LoadOutcome::Failed(format!("{err:#}"))
            }
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&emit.summary(&load))?);
    } else {
        emit.print_human(&load);
    }

    if matches!(load, LoadOutcome::Failed(_)) {
        process::exit(3);
    }
    Ok(())
}

/// DB-free core of `analyze`: resolve scope → parse → write IR + GraphArtifacts.
/// Returns everything the caller needs to load and report. No process exits, no DB.
#[cfg(test)]
pub(crate) fn analyze_emit(scan: &scan::ScanResult, request: ScopeRequest) -> Result<EmitOutcome> {
    analyze_emit_with_options(
        scan,
        request,
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: true,
        },
    )
}

pub(crate) fn analyze_emit_with_options(
    scan: &scan::ScanResult,
    request: ScopeRequest,
    cache: AnalyzeCacheOptions,
) -> Result<EmitOutcome> {
    let scope_file = scope::resolve(&scan.repo_map, &scan.java_files, request)?;
    let scope_path = scope::write_scope_file(&scope_file)?;
    analyze_from_scope_with_options(scope_file, scope_path, &scan.repo_map.jars, cache)
}

/// DB-free core shared by `analyze` and `resolve`: parse the files listed in
/// `scope_file` → resolve → extract JAR API for unresolved types → write IR +
/// GraphArtifacts.
#[cfg(test)]
pub(crate) fn analyze_from_scope(
    scope_file: ScopeFile,
    scope_path: PathBuf,
    jars: &[JarInfo],
) -> Result<EmitOutcome> {
    analyze_from_scope_with_options(
        scope_file,
        scope_path,
        jars,
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: true,
        },
    )
}

pub(crate) fn analyze_from_scope_with_options(
    scope_file: ScopeFile,
    scope_path: PathBuf,
    jars: &[JarInfo],
    cache: AnalyzeCacheOptions,
) -> Result<EmitOutcome> {
    let repo_root = PathBuf::from(&scope_file.repo_root);
    let cih_dir = repo_root.join(".cih");

    let incremental = parse_scope(&repo_root, &cih_dir, &scope_file.files, cache)?;
    if let ParseScopeOutcome::Reused {
        artifacts,
        parsed_files_path,
        node_count,
        edge_count,
        parsed_file_count,
        cache_stats,
    } = incremental
    {
        return Ok(EmitOutcome {
            scope_file,
            scope_path,
            artifacts,
            parsed_files_path,
            artifacts_dir: cih_dir
                .join("artifacts")
                .join(cache_stats.version.as_deref().unwrap_or_default()),
            version: cache_stats.version.clone().unwrap_or_default(),
            node_count,
            edge_count,
            resolved_edge_count: 0,
            unresolved_reference_count: 0,
            unresolved_external_fqcns: Vec::new(),
            parsed_file_count,
            skipped_count: 0,
            jar_node_count: 0,
            jar_failed: 0,
            reused_artifacts: true,
            cache_stats,
        });
    }

    let ParseScopeOutcome::Parsed {
        parse_output,
        current_hashes,
        cache_stats,
    } = incremental
    else {
        unreachable!("reused case returned above");
    };
    let resolve_output = cih_resolve::resolve_edges(&parse_output.parsed_files);
    let (jar_nodes, jar_edges, jar_failed) =
        extract_jar_api(jars, &resolve_output.unresolved_external_fqcns);
    let jar_node_count = jar_nodes.len();

    let mut edges = combined_edges(&parse_output.edges, &resolve_output.edges);
    edges.extend(jar_edges);

    let mut all_nodes = parse_output.nodes;
    all_nodes.extend(jar_nodes);

    let version = content_version(&all_nodes, &edges, &parse_output.parsed_files);

    let parsed_dir = cih_dir.join("parsed").join(&version);
    let parse_artifacts = cih_parse::write_parsed_files(&parsed_dir, &parse_output.parsed_files)?;

    let artifacts_dir = cih_dir.join("artifacts").join(&version);
    let artifacts = GraphArtifacts::write(
        &artifacts_dir,
        VersionId(version.clone()),
        &all_nodes,
        &edges,
    )
    .with_context(|| {
        format!(
            "failed to write graph artifacts to {}",
            artifacts_dir.display()
        )
    })?;

    prune_other_versions(&cih_dir.join("parsed"), &version)?;
    prune_other_versions(&cih_dir.join("artifacts"), &version)?;
    FileHashIndex::from_map(current_hashes).save(&cih_dir)?;

    tracing::info!(
        nodes = all_nodes.len(),
        edges = edges.len(),
        resolved_edges = resolve_output.edges.len(),
        jar_nodes = jar_node_count,
        unresolved_refs = resolve_output.skipped,
        version = %version,
        path = %artifacts_dir.display(),
        "Graph artifacts written"
    );

    let cache_stats = cache_stats.with_version(version.clone());

    Ok(EmitOutcome {
        scope_file,
        scope_path,
        artifacts,
        parsed_files_path: parse_artifacts.parsed_files_path,
        artifacts_dir,
        version,
        node_count: all_nodes.len(),
        edge_count: edges.len(),
        resolved_edge_count: resolve_output.edges.len(),
        unresolved_reference_count: resolve_output.skipped,
        unresolved_external_fqcns: resolve_output.unresolved_external_fqcns,
        parsed_file_count: parse_output.parsed_files.len(),
        skipped_count: parse_output.skipped.len(),
        jar_node_count,
        jar_failed,
        reused_artifacts: false,
        cache_stats,
    })
}

#[derive(Clone, Copy)]
pub(crate) struct AnalyzeCacheOptions {
    pub(crate) use_cache: bool,
    pub(crate) allow_noop: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct CacheStats {
    pub(crate) enabled: bool,
    pub(crate) noop: bool,
    pub(crate) cache_hits: usize,
    pub(crate) changed_files: usize,
    pub(crate) expanded_files: usize,
    pub(crate) reparsed_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
}

impl CacheStats {
    fn with_version(mut self, version: String) -> Self {
        self.version = Some(version);
        self
    }
}

enum ParseScopeOutcome {
    Reused {
        artifacts: GraphArtifacts,
        parsed_files_path: PathBuf,
        node_count: usize,
        edge_count: usize,
        parsed_file_count: usize,
        cache_stats: CacheStats,
    },
    Parsed {
        parse_output: ParseOutput,
        current_hashes: HashMap<String, String>,
        cache_stats: CacheStats,
    },
}

fn parse_scope(
    repo_root: &Path,
    cih_dir: &Path,
    files: &[String],
    cache: AnalyzeCacheOptions,
) -> Result<ParseScopeOutcome> {
    let current_hashes = hash_all(repo_root, files);

    if !cache.use_cache {
        let unit_output = cih_parse::parse_file_units(repo_root, files)?;
        for unit in &unit_output.units {
            if let Some(hash) = current_hashes.get(&unit.rel) {
                save_cached_parsed(cih_dir, hash, unit)?;
            }
        }
        let reparsed_files = unit_output.units.len();
        return Ok(ParseScopeOutcome::Parsed {
            parse_output: cih_parse::parse_output_from_units(
                unit_output.units,
                unit_output.skipped,
            ),
            current_hashes,
            cache_stats: CacheStats {
                enabled: false,
                reparsed_files,
                ..CacheStats::default()
            },
        });
    }

    let previous = FileHashIndex::load(cih_dir);
    let changed_files: Vec<String> = previous
        .changed_files(&current_hashes)
        .into_iter()
        .map(str::to_string)
        .collect();
    let all_files_hashed = current_hashes.len() == files.len();

    if cache.allow_noop
        && all_files_hashed
        && changed_files.is_empty()
        && previous.same_file_set(&current_hashes)
    {
        match reused_artifacts(repo_root, cih_dir) {
            Ok(reused) => {
                tracing::info!("nothing changed, reusing last artifacts");
                return Ok(ParseScopeOutcome::Reused {
                    cache_stats: CacheStats {
                        enabled: true,
                        noop: true,
                        version: Some(reused.artifacts.version.0.clone()),
                        ..CacheStats::default()
                    },
                    artifacts: reused.artifacts,
                    parsed_files_path: reused.parsed_files_path,
                    node_count: reused.node_count,
                    edge_count: reused.edge_count,
                    parsed_file_count: reused.parsed_file_count,
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "incremental no-op unavailable; falling back to parse");
            }
        }
    }

    let mut cached_by_file: HashMap<String, ParsedUnit> = HashMap::new();
    for rel in files {
        let unit = current_hashes
            .get(rel)
            .and_then(|hash| load_cached_parsed(cih_dir, hash))
            .or_else(|| {
                previous
                    .get(rel)
                    .and_then(|hash| load_cached_parsed(cih_dir, hash))
            });
        if let Some(unit) = unit {
            cached_by_file.insert(rel.clone(), unit);
        }
    }

    let cached_parsed: Vec<_> = cached_by_file
        .values()
        .map(|unit| unit.parsed_file.clone())
        .collect();
    let importer_index = ImporterIndex::build(&cached_parsed);
    let mut to_parse: HashSet<String> = importer_index.expand(&changed_files, 4);
    for rel in files {
        if !cached_by_file.contains_key(rel) {
            to_parse.insert(rel.clone());
        }
        if !current_hashes.contains_key(rel) {
            to_parse.insert(rel.clone());
        }
    }

    let mut to_parse: Vec<String> = to_parse.into_iter().collect();
    to_parse.sort();
    let unit_output = cih_parse::parse_file_units(repo_root, &to_parse)?;

    let reparsed_files = unit_output.units.len();
    let skipped_reparse: HashSet<&str> = unit_output
        .skipped
        .iter()
        .map(|skipped| skipped.rel.as_str())
        .collect();
    let mut parsed_by_file: HashMap<String, ParsedUnit> = HashMap::new();
    for unit in unit_output.units {
        if let Some(hash) = current_hashes.get(&unit.rel) {
            save_cached_parsed(cih_dir, hash, &unit)?;
        }
        parsed_by_file.insert(unit.rel.clone(), unit);
    }

    let mut combined_units = Vec::new();
    for rel in files {
        if let Some(unit) = parsed_by_file.remove(rel) {
            combined_units.push(unit);
        } else if !skipped_reparse.contains(rel.as_str()) {
            if let Some(unit) = cached_by_file.remove(rel) {
                combined_units.push(unit);
            }
        }
    }
    let cache_hits = combined_units
        .iter()
        .filter(|unit| !to_parse.iter().any(|rel| rel == &unit.rel))
        .count();
    let expanded_files = to_parse.len();

    Ok(ParseScopeOutcome::Parsed {
        parse_output: cih_parse::parse_output_from_units(combined_units, unit_output.skipped),
        current_hashes,
        cache_stats: CacheStats {
            enabled: true,
            noop: false,
            cache_hits,
            changed_files: changed_files.len(),
            expanded_files,
            reparsed_files,
            version: None,
        },
    })
}

struct ReusedArtifacts {
    artifacts: GraphArtifacts,
    parsed_files_path: PathBuf,
    node_count: usize,
    edge_count: usize,
    parsed_file_count: usize,
}

fn reused_artifacts(repo_root: &Path, cih_dir: &Path) -> Result<ReusedArtifacts> {
    let artifacts = latest_graph_artifacts(repo_root)?;
    let nodes = artifacts.read_nodes()?;
    let edges = artifacts.read_edges()?;
    let parsed_dir = cih_dir.join("parsed").join(&artifacts.version.0);
    let parsed_files_path = parsed_dir.join("parsed-files.jsonl");
    let parsed_file_count = cih_parse::load_parsed_files(&parsed_dir)
        .map(|files| files.len())
        .unwrap_or(0);
    Ok(ReusedArtifacts {
        artifacts,
        parsed_files_path,
        node_count: nodes.len(),
        edge_count: edges.len(),
        parsed_file_count,
    })
}

/// Everything `analyze_emit` produced (DB-free), used to load + report.
pub(crate) struct EmitOutcome {
    pub(crate) scope_file: scope::ScopeFile,
    pub(crate) scope_path: PathBuf,
    pub(crate) artifacts: GraphArtifacts,
    pub(crate) parsed_files_path: PathBuf,
    pub(crate) artifacts_dir: PathBuf,
    pub(crate) version: String,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) resolved_edge_count: usize,
    pub(crate) jar_node_count: usize,
    pub(crate) jar_failed: usize,
    pub(crate) unresolved_reference_count: u64,
    pub(crate) unresolved_external_fqcns: Vec<String>,
    pub(crate) parsed_file_count: usize,
    pub(crate) skipped_count: usize,
    pub(crate) reused_artifacts: bool,
    pub(crate) cache_stats: CacheStats,
}

impl EmitOutcome {
    pub(crate) fn summary<'a>(&'a self, load: &'a LoadOutcome) -> AnalyzeSummary<'a> {
        AnalyzeSummary {
            scope: &self.scope_file,
            version: &self.version,
            scope_path: self.scope_path.display().to_string(),
            parsed_files_path: self.parsed_files_path.display().to_string(),
            artifacts_path: self.artifacts_dir.display().to_string(),
            node_count: self.node_count,
            edge_count: self.edge_count,
            resolved_edge_count: self.resolved_edge_count,
            unresolved_reference_count: self.unresolved_reference_count,
            unresolved_external_fqcns: &self.unresolved_external_fqcns,
            parsed_file_count: self.parsed_file_count,
            skipped_count: self.skipped_count,
            jar_node_count: self.jar_node_count,
            jar_failed: self.jar_failed,
            reused_artifacts: self.reused_artifacts,
            cache: &self.cache_stats,
            falkor_status: load.status(),
            falkor_nodes: load.stats().map(|s| s.nodes),
            falkor_edges: load.stats().map(|s| s.edges),
            falkor_error: load.error(),
        }
    }

    pub(crate) fn print_human(&self, load: &LoadOutcome) {
        println!(
            "Scope: {} .java files across {} modules -> {}.",
            self.scope_file.file_count,
            self.scope_file.modules.len(),
            self.scope_path.display()
        );
        println!(
            "Parsed: {} files -> {} nodes, {} edges, IR {}.",
            self.parsed_file_count,
            self.node_count,
            self.edge_count,
            self.parsed_files_path.display()
        );
        if self.skipped_count > 0 {
            println!(
                "Skipped: {} files (see logs for details).",
                self.skipped_count
            );
        }
        if self.cache_stats.enabled {
            if self.cache_stats.noop {
                println!("Cache: no source changes; reused existing artifacts.");
            } else {
                println!(
                    "Cache: {} hits, {} changed, {} expanded, {} reparsed.",
                    self.cache_stats.cache_hits,
                    self.cache_stats.changed_files,
                    self.cache_stats.expanded_files,
                    self.cache_stats.reparsed_files
                );
            }
        }
        println!(
            "Resolved: {} edges, {} unresolved refs.",
            self.resolved_edge_count, self.unresolved_reference_count
        );
        if !self.unresolved_external_fqcns.is_empty() {
            println!(
                "External unresolved types: {}.",
                self.unresolved_external_fqcns.len()
            );
        }
        if self.jar_node_count > 0 || self.jar_failed > 0 {
            let failed_note = if self.jar_failed > 0 {
                format!(", {} JARs failed", self.jar_failed)
            } else {
                String::new()
            };
            println!(
                "JAR API: {} nodes from dependency JARs{}.",
                self.jar_node_count, failed_note
            );
        }
        println!(
            "Artifacts: {} (version {})",
            self.artifacts_dir.display(),
            self.version
        );
        match load {
            LoadOutcome::Loaded(stats) => {
                println!(
                    "FalkorDB: loaded {} nodes, {} edges.",
                    stats.nodes, stats.edges
                )
            }
            LoadOutcome::Skipped => println!("FalkorDB: skipped (--no-load)."),
            LoadOutcome::Reused => println!("FalkorDB: unchanged; existing live graph reused."),
            LoadOutcome::Failed(_) => {
                println!("FalkorDB: load failed (artifacts on disk — re-run to retry).")
            }
        }
    }
}

#[derive(Serialize)]
pub(crate) struct AnalyzeSummary<'a> {
    pub(crate) scope: &'a scope::ScopeFile,
    pub(crate) version: &'a str,
    pub(crate) scope_path: String,
    pub(crate) parsed_files_path: String,
    pub(crate) artifacts_path: String,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) resolved_edge_count: usize,
    pub(crate) unresolved_reference_count: u64,
    pub(crate) unresolved_external_fqcns: &'a [String],
    pub(crate) parsed_file_count: usize,
    pub(crate) skipped_count: usize,
    pub(crate) jar_node_count: usize,
    pub(crate) jar_failed: usize,
    pub(crate) reused_artifacts: bool,
    pub(crate) cache: &'a CacheStats,
    /// "loaded" | "skipped" | "failed"
    pub(crate) falkor_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) falkor_nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) falkor_edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) falkor_error: Option<&'a str>,
}

/// Extract API-surface nodes+edges from `jars` for the given FQCN set.
/// Demand-driven: passes `include` to [`JarApiExtractor::with_include`] so only
/// classes matching an unresolved FQCN are parsed. Returns (nodes, edges, failed_jar_count).
pub(crate) fn extract_jar_api(jars: &[JarInfo], fqcns: &[String]) -> (Vec<Node>, Vec<Edge>, usize) {
    if fqcns.is_empty() || jars.is_empty() {
        return (Vec::new(), Vec::new(), 0);
    }
    let include: std::collections::HashSet<String> = fqcns.iter().cloned().collect();
    let extractor = JarApiExtractor::with_include(include);
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut failed = 0usize;
    for jar in jars {
        match extractor.extract(std::path::Path::new(&jar.path)) {
            Ok(output) => {
                all_nodes.extend(output.nodes);
                all_edges.extend(output.edges);
            }
            Err(err) => {
                tracing::warn!(jar = %jar.path, error = %err, "JAR API extraction failed — skipping");
                failed += 1;
            }
        }
    }
    (all_nodes, all_edges, failed)
}

/// Read `.cih/repo-map.json` and return its JAR catalog. Returns an empty vec
/// if the file is absent or malformed (graceful no-op when `scan` hasn't run).
fn load_jars_from_repo_map(repo: &Path) -> Vec<JarInfo> {
    let path = repo.join(".cih").join("repo-map.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<RepoMap>(&raw)
        .map(|rm| rm.jars)
        .unwrap_or_default()
}

fn combined_edges(structure: &[Edge], resolved: &[Edge]) -> Vec<Edge> {
    let mut map: std::collections::BTreeMap<(String, String, &'static str), Edge> =
        std::collections::BTreeMap::new();
    for edge in structure.iter().chain(resolved.iter()).cloned() {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        );
        match map.entry(key) {
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if edge.confidence > slot.get().confidence {
                    *slot.get_mut() = edge;
                }
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(edge);
            }
        }
    }
    map.into_values().collect()
}

fn build_scope_request(repo: &std::path::Path, flags: &AnalyzeFlags) -> Result<ScopeRequest> {
    let scope_path = if let Some(path) = &flags.scope {
        Some(path.clone())
    } else {
        let default = repo.join("cih.scope.toml");
        default.exists().then_some(default)
    };

    let mut request = if let Some(path) = scope_path {
        ScopeRequest::from_toml(&path)?
    } else {
        ScopeRequest::default()
    };

    if flags.all {
        request.all = true;
        request.modules.clear();
        request.include.clear();
    } else if !flags.modules.is_empty() {
        request.all = false;
        request.modules = flags.modules.clone();
        request.include.clear();
    } else if !flags.include.is_empty() {
        request.all = false;
        request.modules.clear();
        request.include = flags.include.clone();
    }

    if !flags.exclude.is_empty() {
        request.exclude = flags.exclude.clone();
    }
    if flags.include_decompiled {
        request.include_decompiled = true;
    }

    Ok(request)
}
