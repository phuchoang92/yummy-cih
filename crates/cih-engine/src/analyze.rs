use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{Edge, GraphArtifacts, JarInfo, Node, RepoMap, VersionId};
use cih_jar::JarApiExtractor;
use serde::Serialize;

use crate::db::{load_to_falkor, LoadOutcome};
use crate::scope::{self, ScopeFile, ScopeRequest};
use crate::versioning::{content_version, prune_other_versions};
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

    let emit = analyze_emit(&scan, request)?;

    let load = if flags.no_load {
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
    let emit = analyze_from_scope(scope_file, scope_path, &jars)?;

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
pub(crate) fn analyze_emit(scan: &scan::ScanResult, request: ScopeRequest) -> Result<EmitOutcome> {
    let scope_file = scope::resolve(&scan.repo_map, &scan.java_files, request)?;
    let scope_path = scope::write_scope_file(&scope_file)?;
    analyze_from_scope(scope_file, scope_path, &scan.repo_map.jars)
}

/// DB-free core shared by `analyze` and `resolve`: parse the files listed in
/// `scope_file` → resolve → extract JAR API for unresolved types → write IR +
/// GraphArtifacts.
pub(crate) fn analyze_from_scope(
    scope_file: ScopeFile,
    scope_path: PathBuf,
    jars: &[JarInfo],
) -> Result<EmitOutcome> {
    let repo_root = PathBuf::from(&scope_file.repo_root);

    let parse_output = cih_parse::parse_files(&repo_root, &scope_file.files)?;
    let resolve_output = cih_resolve::resolve_edges(&parse_output.parsed_files);
    let (jar_nodes, jar_edges, jar_failed) =
        extract_jar_api(jars, &resolve_output.unresolved_external_fqcns);
    let jar_node_count = jar_nodes.len();

    let mut edges = combined_edges(&parse_output.edges, &resolve_output.edges);
    edges.extend(jar_edges);

    let mut all_nodes = parse_output.nodes;
    all_nodes.extend(jar_nodes);

    let version = content_version(&all_nodes, &edges, &parse_output.parsed_files);

    let cih_dir = repo_root.join(".cih");
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
