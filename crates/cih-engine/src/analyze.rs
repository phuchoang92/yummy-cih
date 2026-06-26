use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{self, Edge, GraphArtifacts, JarInfo, Node, RepoMap, VersionId};
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
pub struct AnalyzeFlags {
    pub all: bool,
    pub modules: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_decompiled: bool,
    pub scope: Option<PathBuf>,
    pub json: bool,
    pub falkor_url: Option<String>,
    pub graph_key: Option<String>,
    pub no_load: bool,
    pub no_cache: bool,
    /// Skip the integration + DI XML walk (faster on large repos).
    pub skip_xml_integration: bool,
    /// Language filter: only include files for these languages (empty = all).
    pub languages: Vec<String>,
}

pub fn run_analyze(repo: PathBuf, flags: AnalyzeFlags) -> Result<()> {
    let span = tracing::info_span!("analyze", repo = %repo.display());
    let _enter = span.enter();

    tracing::info!(repo = %repo.display(), "starting analyze");

    let scan = scan::scan_repo(&repo)?;
    let repo_map_path = scan::write_repo_map(&scan.repo_map)?;
    tracing::info!(
        path = %repo_map_path.display(),
        source_files = scan.repo_map.total_source_files,
        modules = scan.repo_map.modules.len(),
        "repo-map written"
    );

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
            skip_xml_integration: flags.skip_xml_integration,
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
        emit.print_styled(&load);
    }

    let graph_key = flags.graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
    crate::registry::persist_analyze(&emit, graph_key);

    if matches!(load, LoadOutcome::Failed(_)) {
        process::exit(3);
    }
    Ok(())
}

pub fn run_resolve(
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
            skip_xml_integration: false,
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
        emit.print_styled(&load);
    }

    if matches!(load, LoadOutcome::Failed(_)) {
        process::exit(3);
    }
    Ok(())
}

/// DB-free core of `analyze`: resolve scope → parse → write IR + GraphArtifacts.
/// Returns everything the caller needs to load and report. No process exits, no DB.
pub fn analyze_emit(scan: &scan::ScanResult, request: ScopeRequest) -> Result<EmitOutcome> {
    analyze_emit_with_options(
        scan,
        request,
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: true,
            skip_xml_integration: false,
        },
    )
}

pub fn analyze_emit_with_options(
    scan: &scan::ScanResult,
    request: ScopeRequest,
    cache: AnalyzeCacheOptions,
) -> Result<EmitOutcome> {
    let scope_file = scope::resolve(&scan.repo_map, &scan.source_files, request)?;
    let scope_path = scope::write_scope_file(&scope_file)?;
    analyze_from_scope_with_options(scope_file, scope_path, &scan.repo_map.jars, cache)
}

/// DB-free core shared by `analyze` and `resolve`: parse the files listed in
/// `scope_file` → resolve → extract JAR API for unresolved types → write IR +
/// GraphArtifacts.
pub fn analyze_from_scope(
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
            skip_xml_integration: false,
        },
    )
}

pub fn analyze_from_scope_with_options(
    scope_file: ScopeFile,
    scope_path: PathBuf,
    jars: &[JarInfo],
    cache: AnalyzeCacheOptions,
) -> Result<EmitOutcome> {
    let repo_root = PathBuf::from(&scope_file.repo_root);
    let cih_dir = repo_root.join(".cih");
    let mut ui = crate::ui::PhaseProgress::new();

    // Gap 5: auto-bootstrap hint — if a bundle archive exists and file-hashes.json
    // does not, suggest using `cih-engine artifact bootstrap` to restore state.
    let bundle_path = cih_dir.join("graph.db.zst");
    let hashes_path = cih_dir.join("file-hashes.json");
    if bundle_path.exists() && !hashes_path.exists() {
        tracing::info!(
            bundle = %bundle_path.display(),
            "Bundle archive found but no incremental state — run `cih-engine artifact bootstrap` \
             to restore; this analyze will be a full re-parse"
        );
    }

    tracing::info!(
        files = scope_file.files.len(),
        modules = scope_file.modules.len(),
        cache_enabled = cache.use_cache,
        "starting parse phase"
    );
    ui.spin(format!("Parsing {} files", scope_file.files.len()));

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
        ui.finish_with(format!(
            "{} nodes, {} edges  \x1b[2m(no changes — reused)\x1b[0m",
            crate::ui::fmt_count(node_count),
            crate::ui::fmt_count(edge_count)
        ));
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
            unresolved_report_path: None,
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

    tracing::info!(
        parsed = parse_output.parsed_files.len(),
        skipped = parse_output.skipped.len(),
        struct_nodes = parse_output.nodes.len(),
        struct_edges = parse_output.edges.len(),
        "parse phase complete"
    );

    // ── Resolve ───────────────────────────────────────────────────────────────
    tracing::info!("starting resolve phase");
    ui.finish_with(format!(
        "{} parsed, {} skipped",
        crate::ui::fmt_count(parse_output.parsed_files.len()),
        crate::ui::fmt_count(parse_output.skipped.len())
    ));
    ui.spin("Resolving");

    let resolvers = cih_resolve::default_registry();

    // Gap 4: build Java constant resolver from all parsed files.
    let java_const_resolver = cih_resolve::build_java_constant_resolver(&parse_output.parsed_files);

    let resolve_output = cih_resolve::resolve_with_registry(
        &parse_output.parsed_files,
        &resolvers,
        cih_resolve::ResolveOptions {
            repo_root: Some(&repo_root),
            enable_xml_integrations: !cache.skip_xml_integration,
            constant_resolver: Some(Box::new(java_const_resolver)),
        },
    );
    tracing::info!(
        resolved_edges = resolve_output.edges.len(),
        skipped_refs = resolve_output.skipped,
        unresolved_fqcns = resolve_output.unresolved_external_fqcns.len(),
        "resolve phase complete"
    );
    ui.finish_with(format!(
        "{} edges  \x1b[2m({} unresolved)\x1b[0m",
        crate::ui::fmt_count(resolve_output.edges.len()),
        crate::ui::fmt_count(resolve_output.skipped as usize)
    ));

    // ── JAR API extraction ────────────────────────────────────────────────────
    if !jars.is_empty() {
        ui.spin(format!("JAR API ({} JARs)", jars.len()));
    }
    tracing::info!(
        jars = jars.len(),
        unresolved_fqcns = resolve_output.unresolved_external_fqcns.len(),
        "starting JAR API extraction"
    );
    let (jar_nodes, jar_edges, jar_failed) =
        extract_jar_api(jars, &resolve_output.unresolved_external_fqcns);
    let jar_node_count = jar_nodes.len();
    tracing::info!(
        jar_nodes = jar_node_count,
        jar_edges = jar_edges.len(),
        jar_failed,
        "JAR API extraction complete"
    );
    if !jars.is_empty() {
        ui.finish_with(format!("{} nodes", crate::ui::fmt_count(jar_node_count)));
    }

    // ── DB access + XML integration + artifact write ──────────────────────────
    ui.spin("Writing artifacts");

    let (mut db_nodes, mut db_edges) = cih_resolve::emit_db_access(&parse_output.parsed_files);
    let (jpa_nodes, jpa_edges) = cih_resolve::emit_jpa_tables(&parse_output.nodes);
    db_nodes.extend(jpa_nodes);
    db_edges.extend(jpa_edges);
    tracing::info!(
        db_query_nodes = db_nodes
            .iter()
            .filter(|n| n.kind == cih_core::NodeKind::DbQuery)
            .count(),
        db_table_nodes = db_nodes
            .iter()
            .filter(|n| n.kind == cih_core::NodeKind::DbTable)
            .count(),
        "DB access emit complete"
    );

    let has_java = scope_file.files.iter().any(|f| f.ends_with(".java"));
    let (xml_nodes, xml_edges) = if cache.skip_xml_integration || !has_java {
        if !has_java {
            tracing::info!("Skipping XML integration + DI extraction (no Java files in scope)");
        } else {
            tracing::info!("Skipping XML integration + DI extraction (--skip-xml-integration)");
        }
        (Vec::new(), Vec::new())
    } else {
        let (mut xml_nodes, mut xml_edges) = extract_integration_xml_in_repo(&repo_root);
        tracing::info!(
            integration_route_nodes = xml_nodes
                .iter()
                .filter(|n| n.kind == cih_core::NodeKind::IntegrationRoute)
                .count(),
            message_destination_nodes = xml_nodes
                .iter()
                .filter(|n| n.kind == cih_core::NodeKind::MessageDestination)
                .count(),
            integration_edges = xml_edges.len(),
            "integration XML extraction complete"
        );

        // Phase 2a — Spring/Blueprint DI resolution via registry (JavaResolver::extra_edges).
        let (di_nodes, di_edges) =
            resolvers.extra_edges(Some(&repo_root), &parse_output.parsed_files);
        tracing::info!(
            di_bean_nodes = di_nodes.len(),
            di_calls_edges = di_edges.len(),
            "DI XML extraction complete"
        );
        xml_nodes.extend(di_nodes);
        xml_edges.extend(di_edges);
        (xml_nodes, xml_edges)
    };

    let mut edges = combined_edges(&parse_output.edges, &resolve_output.edges);
    edges.extend(jar_edges);
    edges.extend(db_edges);
    edges.extend(xml_edges);

    let mut all_nodes = parse_output.nodes;
    all_nodes.extend(resolve_output.nodes);
    all_nodes.extend(jar_nodes);
    all_nodes.extend(db_nodes);
    all_nodes.extend(xml_nodes);

    // Gap 1: propagate transitive loop depths along CALLS edges.
    cih_resolve::propagate_loop_depths(&mut all_nodes, &edges);

    // Gap 2: emit SIMILAR_TO edges from MinHash body fingerprints.
    let similar_edges = cih_resolve::emit_similar_to_edges(&all_nodes);
    if !similar_edges.is_empty() {
        tracing::info!(
            similar_to_edges = similar_edges.len(),
            "MinHash near-clone edges emitted"
        );
        edges.extend(similar_edges);
    }

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

    cih_resolve::write_unresolved_reports(&resolve_output.unresolved_refs, &artifacts_dir)
        .with_context(|| "failed to write unresolved-refs reports")?;

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
    ui.finish_with(format!(
        "{} nodes, {} edges  \x1b[2m(v{})\x1b[0m",
        crate::ui::fmt_count(all_nodes.len()),
        crate::ui::fmt_count(edges.len()),
        &version[..8.min(version.len())]
    ));

    let cache_stats = cache_stats.with_version(version.clone());

    Ok(EmitOutcome {
        scope_file,
        scope_path,
        artifacts,
        parsed_files_path: parse_artifacts.parsed_files_path,
        artifacts_dir: artifacts_dir.clone(),
        version,
        node_count: all_nodes.len(),
        edge_count: edges.len(),
        resolved_edge_count: resolve_output.edges.len(),
        unresolved_reference_count: resolve_output.skipped,
        unresolved_external_fqcns: resolve_output.unresolved_external_fqcns,
        unresolved_report_path: Some(artifacts_dir.join("unresolved-refs.md")),
        parsed_file_count: parse_output.parsed_files.len(),
        skipped_count: parse_output.skipped.len(),
        jar_node_count,
        jar_failed,
        reused_artifacts: false,
        cache_stats,
    })
}

#[derive(Clone, Copy)]
pub struct AnalyzeCacheOptions {
    pub use_cache: bool,
    pub allow_noop: bool,
    /// Skip the integration + DI XML walk (faster on large repos).
    pub skip_xml_integration: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheStats {
    pub enabled: bool,
    pub noop: bool,
    pub cache_hits: usize,
    pub changed_files: usize,
    pub expanded_files: usize,
    pub reparsed_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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

fn default_registry() -> cih_parse::LanguageRegistry {
    scan::default_scan_registry()
}

fn parse_scope(
    repo_root: &Path,
    cih_dir: &Path,
    files: &[String],
    cache: AnalyzeCacheOptions,
) -> Result<ParseScopeOutcome> {
    tracing::info!(
        total_files = files.len(),
        cache_enabled = cache.use_cache,
        "hashing files"
    );
    let current_hashes = hash_all(repo_root, files);
    tracing::debug!(hashed = current_hashes.len(), "file hashing complete");

    if !cache.use_cache {
        tracing::info!(files = files.len(), "cache disabled — parsing all files");
        let unit_output = cih_parse::parse_file_units(repo_root, files, &default_registry())?;
        for unit in &unit_output.units {
            if let Some(hash) = current_hashes.get(&unit.rel) {
                save_cached_parsed(cih_dir, hash, unit)?;
            }
        }
        let reparsed_files = unit_output.units.len();
        tracing::info!(
            reparsed = reparsed_files,
            skipped = unit_output.skipped.len(),
            "parse complete (no-cache)"
        );
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

    tracing::info!(
        changed = changed_files.len(),
        total = files.len(),
        "incremental cache check complete"
    );
    if !changed_files.is_empty() {
        for f in changed_files.iter().take(20) {
            tracing::debug!(file = %f, "changed");
        }
        if changed_files.len() > 20 {
            tracing::debug!("... and {} more changed files", changed_files.len() - 20);
        }
    }

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

    let cache_hits_pre = files.len().saturating_sub(to_parse.len());
    tracing::info!(
        to_parse = to_parse.len(),
        cache_hits = cache_hits_pre,
        changed = changed_files.len(),
        "incremental parse: {} files to parse, {} from cache",
        to_parse.len(),
        cache_hits_pre,
    );

    let unit_output = cih_parse::parse_file_units(repo_root, &to_parse, &default_registry())?;
    tracing::info!(
        parsed = unit_output.units.len(),
        skipped = unit_output.skipped.len(),
        "parse units complete"
    );

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
pub struct EmitOutcome {
    pub scope_file: scope::ScopeFile,
    pub scope_path: PathBuf,
    pub artifacts: GraphArtifacts,
    pub parsed_files_path: PathBuf,
    pub artifacts_dir: PathBuf,
    pub version: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub resolved_edge_count: usize,
    pub jar_node_count: usize,
    pub jar_failed: usize,
    pub unresolved_reference_count: u64,
    pub unresolved_external_fqcns: Vec<String>,
    pub unresolved_report_path: Option<PathBuf>,
    pub parsed_file_count: usize,
    pub skipped_count: usize,
    pub reused_artifacts: bool,
    pub cache_stats: CacheStats,
}

impl EmitOutcome {
    pub fn summary<'a>(&'a self, load: &'a LoadOutcome) -> AnalyzeSummary<'a> {
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

    pub fn print_styled(&self, load: &LoadOutcome) {
        let repo_name = Path::new(&self.scope_file.repo_root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let ver = &self.version[..8.min(self.version.len())];
        crate::ui::print_header("Analyze", repo_name, Some(ver));
        crate::ui::print_row(
            "Files",
            &format!(
                "{} parsed{}",
                crate::ui::fmt_count(self.parsed_file_count),
                if self.skipped_count > 0 {
                    format!("  \x1b[2m({} skipped)\x1b[0m", self.skipped_count)
                } else {
                    String::new()
                }
            ),
        );
        crate::ui::print_row(
            "Graph",
            &format!(
                "{}  nodes  {}  edges",
                crate::ui::fmt_count(self.node_count),
                crate::ui::fmt_count(self.edge_count)
            ),
        );
        if self.resolved_edge_count > 0 {
            crate::ui::print_row(
                "Resolve",
                &format!("{}  edges", crate::ui::fmt_count(self.resolved_edge_count)),
            );
        }
        if self.jar_node_count > 0 {
            crate::ui::print_row(
                "JARs",
                &format!("{}  nodes", crate::ui::fmt_count(self.jar_node_count)),
            );
        }
        crate::ui::print_row("Artifacts", &self.artifacts_dir.display().to_string());
        let falkor_str = match load {
            LoadOutcome::Loaded(stats) => {
                format!(
                    "{}  nodes  {}  edges",
                    crate::ui::fmt_count(stats.nodes as usize),
                    crate::ui::fmt_count(stats.edges as usize)
                )
            }
            LoadOutcome::Skipped => "\x1b[2mskipped (--no-load)\x1b[0m".to_string(),
            LoadOutcome::Reused => "\x1b[2mreused (no changes)\x1b[0m".to_string(),
            LoadOutcome::Failed(e) => format!("\x1b[31mfailed\x1b[0m  \x1b[2m{e}\x1b[0m"),
        };
        crate::ui::print_row("FalkorDB", &falkor_str);
        eprintln!();
    }
}

#[derive(Serialize)]
pub struct AnalyzeSummary<'a> {
    pub scope: &'a scope::ScopeFile,
    pub version: &'a str,
    pub scope_path: String,
    pub parsed_files_path: String,
    pub artifacts_path: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub resolved_edge_count: usize,
    pub unresolved_reference_count: u64,
    pub unresolved_external_fqcns: &'a [String],
    pub parsed_file_count: usize,
    pub skipped_count: usize,
    pub jar_node_count: usize,
    pub jar_failed: usize,
    pub reused_artifacts: bool,
    pub cache: &'a CacheStats,
    /// "loaded" | "skipped" | "failed"
    pub falkor_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falkor_nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falkor_edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falkor_error: Option<&'a str>,
}

/// Extract API-surface nodes+edges from `jars` for the given FQCN set.
/// Demand-driven: passes `include` to [`JarApiExtractor::with_include`] so only
/// classes matching an unresolved FQCN are parsed. Returns (nodes, edges, failed_jar_count).
pub fn extract_jar_api(jars: &[JarInfo], fqcns: &[String]) -> (Vec<Node>, Vec<Edge>, usize) {
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

/// Scan the repo for `*.xml` files and run the integration-XML extractor on each.
/// Best-effort: unreadable files are skipped with a warning, never fatal. Nodes
/// are deduplicated by id (e.g. the same MessageDestination referenced twice).
pub fn extract_integration_xml_in_repo(repo_root: &Path) -> (Vec<Node>, Vec<Edge>) {
    use rayon::prelude::*;
    use std::collections::HashSet;

    // Collect XML file paths sequentially — the ignore walker is not Sync.
    let xml_files: Vec<PathBuf> = {
        let walker = ignore::WalkBuilder::new(repo_root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .build();

        walker
            .filter_map(|result| match result {
                Ok(entry) if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) => {
                    let path = entry.into_path();
                    let is_xml = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("xml"))
                        .unwrap_or(false);
                    if is_xml {
                        Some(path)
                    } else {
                        None
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "integration-xml: walk error — skipping");
                    None
                }
                _ => None,
            })
            .collect()
    };

    // Read and parse XML files in parallel.
    let per_file: Vec<_> = xml_files
        .par_iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(repo_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(file = %rel, error = %err, "integration-xml: read failed — skipping");
                    return None;
                }
            };
            let output = cih_resolve::extract_integration_xml(&rel, &content);
            if output.nodes.is_empty() && output.edges.is_empty() {
                None
            } else {
                Some(output)
            }
        })
        .collect();

    // Merge sequentially — dedup node IDs across files.
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_node_ids: HashSet<String> = HashSet::new();
    for output in per_file {
        for node in output.nodes {
            if seen_node_ids.insert(node.id.as_str().to_string()) {
                nodes.push(node);
            }
        }
        edges.extend(output.edges);
    }

    (nodes, edges)
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
    let mut map: HashMap<(String, String, &'static str), Edge> =
        HashMap::with_capacity(structure.len() + resolved.len());
    for edge in structure.iter().chain(resolved.iter()) {
        let key = (
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        );
        match map.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let winner = slot.get_mut();
                // Always merge call_sites from the incoming edge (Gap 3).
                merge_call_sites(winner, edge);
                if edge.confidence > winner.confidence {
                    // Promote confidence/reason but keep the merged props.
                    let merged_props = winner.props.take();
                    *winner = edge.clone();
                    winner.props = merged_props;
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(edge.clone());
            }
        }
    }
    let mut result: Vec<Edge> = map.into_values().collect();
    result.sort_unstable_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then_with(|| a.dst.as_str().cmp(b.dst.as_str()))
            .then_with(|| a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    result
}

/// Merge `call_sites` from `incoming` into `winner` (Gap 3).
/// Caps total call-site records at 20 per edge.
fn merge_call_sites(winner: &mut Edge, incoming: &Edge) {
    let Some(incoming_props) = &incoming.props else {
        return;
    };
    let Some(incoming_arr) = incoming_props.get("call_sites").and_then(|v| v.as_array()) else {
        return;
    };
    if incoming_arr.is_empty() {
        return;
    }
    let entry = winner
        .props
        .get_or_insert_with(|| serde_json::json!({"call_sites": []}));
    let existing = entry
        .get_mut("call_sites")
        .and_then(|v| v.as_array_mut())
        .expect("call_sites must be an array");
    existing.extend(incoming_arr.iter().cloned());
    existing.truncate(20);
}

#[cfg(test)]
mod combined_edges_tests {
    use super::*;
    use cih_core::{EdgeKind, NodeId};

    fn edge(src: &str, dst: &str, kind: EdgeKind, confidence: f32) -> Edge {
        Edge {
            src: NodeId::new(src),
            dst: NodeId::new(dst),
            kind,
            confidence,
            reason: String::new(),
            props: None,
        }
    }

    #[test]
    fn deterministic_order_regardless_of_input_order() {
        let a = edge("A", "B", EdgeKind::Calls, 1.0);
        let b = edge("C", "D", EdgeKind::Calls, 1.0);
        let forward = combined_edges(&[a.clone(), b.clone()], &[]);
        let backward = combined_edges(&[b.clone(), a.clone()], &[]);
        let keys = |v: &[Edge]| {
            v.iter()
                .map(|e| (e.src.as_str().to_string(), e.dst.as_str().to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&forward), keys(&backward));
    }

    #[test]
    fn highest_confidence_wins() {
        let low = edge("A", "B", EdgeKind::Calls, 0.5);
        let high = edge("A", "B", EdgeKind::Calls, 0.9);
        let result = combined_edges(&[low], &[high]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].confidence, 0.9);
    }

    #[test]
    fn equal_confidence_retains_first() {
        let first = Edge {
            src: NodeId::new("A"),
            dst: NodeId::new("B"),
            kind: EdgeKind::Calls,
            confidence: 0.7,
            reason: "first".into(),
            props: None,
        };
        let second = Edge {
            src: NodeId::new("A"),
            dst: NodeId::new("B"),
            kind: EdgeKind::Calls,
            confidence: 0.7,
            reason: "second".into(),
            props: None,
        };
        let result = combined_edges(&[first], &[second]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].reason, "first");
    }

    // ---------------------------------------------------------------------------
    // Performance comparison: HashMap (current) vs BTreeMap (old)
    // Run with: cargo test --release -p cih-engine -- bench_combined_edges --nocapture
    // ---------------------------------------------------------------------------
    fn btreemap_combined_edges(structure: &[Edge], resolved: &[Edge]) -> Vec<Edge> {
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

    fn make_edges(n_unique: usize, dup_factor: usize) -> Vec<Edge> {
        let mut v = Vec::with_capacity(n_unique * dup_factor);
        for i in 0..n_unique {
            for d in 0..dup_factor {
                v.push(Edge {
                    src: NodeId::new(format!("com.example.pkg{}.Class{}A", i / 100, i)),
                    dst: NodeId::new(format!("com.example.pkg{}.Class{}B", i / 100, i)),
                    kind: EdgeKind::Calls,
                    confidence: (d as f32) / (dup_factor as f32),
                    reason: String::new(),
                    props: None,
                });
            }
        }
        v
    }

    #[test]
    fn bench_combined_edges() {
        // ~2M edges total (200k unique × 10 dups) — representative of a large repo.
        let edges = make_edges(200_000, 10);
        let mid = edges.len() / 2;
        let structure = &edges[..mid];
        let resolved = &edges[mid..];

        const ITERS: u32 = 5;

        // Warm up
        let _ = combined_edges(structure, resolved);
        let _ = btreemap_combined_edges(structure, resolved);

        let t0 = std::time::Instant::now();
        for _ in 0..ITERS {
            std::hint::black_box(combined_edges(structure, resolved));
        }
        let hashmap_ms = t0.elapsed().as_millis() / ITERS as u128;

        let t1 = std::time::Instant::now();
        for _ in 0..ITERS {
            std::hint::black_box(btreemap_combined_edges(structure, resolved));
        }
        let btreemap_ms = t1.elapsed().as_millis() / ITERS as u128;

        // Correctness: both must produce the same result.
        let hm = combined_edges(structure, resolved);
        let bt = btreemap_combined_edges(structure, resolved);
        assert_eq!(hm.len(), bt.len(), "output length mismatch");
        for (h, b) in hm.iter().zip(bt.iter()) {
            assert_eq!(h.src.as_str(), b.src.as_str(), "src mismatch");
            assert_eq!(h.dst.as_str(), b.dst.as_str(), "dst mismatch");
            assert_eq!(
                h.kind.cypher_label(),
                b.kind.cypher_label(),
                "kind mismatch"
            );
            assert!(
                (h.confidence - b.confidence).abs() < f32::EPSILON,
                "confidence mismatch at {} → {}: {} vs {}",
                h.src.as_str(),
                h.dst.as_str(),
                h.confidence,
                b.confidence
            );
        }

        println!(
            "\ncombined_edges ({} unique, {} total edges, {} iters each):",
            200_000,
            edges.len(),
            ITERS
        );
        println!("  HashMap + sort : {}ms avg", hashmap_ms);
        println!("  BTreeMap       : {}ms avg", btreemap_ms);
        if btreemap_ms > 0 {
            println!(
                "  Speedup        : {:.2}x",
                btreemap_ms as f64 / hashmap_ms as f64
            );
        }
    }
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
    if !flags.languages.is_empty() {
        request.languages = flags.languages.clone();
    }

    Ok(request)
}
