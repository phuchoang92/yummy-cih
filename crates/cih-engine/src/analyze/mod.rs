use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{GraphArtifacts, JarInfo, VersionId};
use serde::Serialize;

use crate::db::{load_to_falkor, LoadOutcome};
use crate::scope::{self, ScopeFile, ScopeRequest};
use crate::versioning::{content_version, prune_other_versions};
use crate::{scan, DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

use cache::{parse_scope, ParseScopeOutcome};
use extract::{build_scope_request, load_jars_from_repo_map};
use merge::combined_edges;

mod cache;
mod extract;
mod merge;

pub use extract::{extract_integration_xml_in_repo, extract_jar_api};

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
    /// Explicit CXF servlet base path (e.g. `/rest`) for `<jaxrs:server>` routes.
    pub cxf_base_path: Option<String>,
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
            cxf_base_path: flags.cxf_base_path.clone(),
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
    // No CLI flags on `resolve`; honor the repo/home cih.toml layers for the base path.
    let cxf_base_path = {
        let layers = crate::settings::Layers::load(&repo);
        layers
            .repo
            .analyze
            .cxf_base_path
            .clone()
            .or_else(|| layers.home.analyze.cxf_base_path.clone())
    };
    let emit = analyze_from_scope_with_options(
        scope_file,
        scope_path,
        &jars,
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: false,
            skip_xml_integration: false,
            cxf_base_path,
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
pub fn analyze_emit(scan: &scan::ScanResult, request: ScopeRequest) -> Result<EmitOutcome> {
    analyze_emit_with_options(
        scan,
        request,
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: true,
            skip_xml_integration: false,
            cxf_base_path: None,
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

/// DB-free core shared by `analyze` and `resolve`: parse → resolve → write artifacts.
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
            cxf_base_path: None,
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

    let bundle_path = cih_dir.join("graph.db.zst");
    let hashes_path = cih_dir.join("file-hashes.json");
    if bundle_path.exists() && !hashes_path.exists() {
        tracing::info!(
            bundle = %bundle_path.display(),
            "Bundle archive found but no incremental state — run `cih-engine artifact bootstrap` \
             to restore; this analyze will be a full re-parse"
        );
    }

    // ── Decompile pre-step ────────────────────────────────────────────────
    let decompile_cfg = crate::decompile_config::DecompileConfig::load_or_default(&repo_root);
    let extra_java_files: Vec<String> = if decompile_cfg.is_enabled() {
        let jars = decompile_cfg.collect_jars(&repo_root);
        ui.start_phase("Decompiling JARs", Some(jars.len() as u64));
        match crate::decompile::run_decompile_precheck(&repo_root, &decompile_cfg, jars, &ui) {
            Ok((dirs, _stats)) => {
                ui.finish_phase();
                crate::decompile::collect_decompiled_java_files(&repo_root, &dirs)
            }
            Err(err) => {
                tracing::warn!(error = %err, "decompile pre-step failed — continuing without decompiled JARs");
                ui.finish_with("decompile failed — skipped");
                vec![]
            }
        }
    } else {
        vec![]
    };

    let combined_files: Vec<String>;
    let files_to_parse: &[String] = if extra_java_files.is_empty() {
        &scope_file.files
    } else {
        combined_files = scope_file
            .files
            .iter()
            .cloned()
            .chain(extra_java_files)
            .collect();
        &combined_files
    };
    // ─────────────────────────────────────────────────────────────────────

    tracing::info!(
        files = files_to_parse.len(),
        modules = scope_file.modules.len(),
        cache_enabled = cache.use_cache,
        "starting parse phase"
    );
    ui.spin(format!("Parsing {} files", files_to_parse.len()));

    let incremental = parse_scope(&repo_root, &cih_dir, files_to_parse, cache.clone())?;
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

    tracing::info!("starting resolve phase");
    ui.finish_with(format!(
        "{} parsed, {} skipped",
        crate::ui::fmt_count(parse_output.parsed_files.len()),
        crate::ui::fmt_count(parse_output.skipped.len())
    ));
    ui.spin("Resolving");

    let resolvers = cih_resolve::default_registry();

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

    // JAR API extraction is JVM-only: JARs come solely from Maven/Gradle repo-maps.
    // A non-JVM repo (JS/TS/Python) has no JARs, so skip the phase and its logs
    // entirely rather than running/logging a no-op.
    let (jar_nodes, jar_edges, jar_node_count, jar_failed) = if jars.is_empty() {
        (Vec::new(), Vec::new(), 0, 0)
    } else {
        ui.spin(format!("JAR API ({} JARs)", jars.len()));
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
        ui.finish_with(format!("{} nodes", crate::ui::fmt_count(jar_node_count)));
        (jar_nodes, jar_edges, jar_node_count, jar_failed)
    };

    ui.spin("Writing artifacts");

    // DB access (SQL sites/constants) + JPA (@Entity tables) — only some languages
    // produce these (Java; TS/JS/Python emit none). Skip the phase + log when the
    // parse output contains nothing to emit, so it's silent for non-DB repos.
    let has_sql = parse_output
        .parsed_files
        .iter()
        .any(|f| !f.sql_execution_sites.is_empty() || !f.sql_constants.is_empty());
    // Mirror emit_jpa_tables' gate exactly (stereotype == "entity" on a
    // Class/Interface/Record) so has_jpa is a true superset — never skips a repo
    // that would produce JPA table nodes.
    let has_jpa = parse_output.nodes.iter().any(|n| {
        matches!(
            n.kind,
            cih_core::NodeKind::Class | cih_core::NodeKind::Interface | cih_core::NodeKind::Record
        ) && n
            .props
            .as_ref()
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str())
            == Some("entity")
    });
    let (db_nodes, db_edges) = if has_sql || has_jpa {
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
        (db_nodes, db_edges)
    } else {
        (Vec::new(), Vec::new())
    };

    let has_java = scope_file.files.iter().any(|f| f.ends_with(".java"));
    let (xml_nodes, xml_edges) = if cache.skip_xml_integration || !has_java {
        // Debug-level: a non-Java analyze shouldn't advertise skipped Java phases.
        if !has_java {
            tracing::debug!("Skipping XML integration + DI extraction (no Java files in scope)");
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

    // Language-specific post-processing over the assembled graph (e.g. the Java resolver
    // rewrites HTTP route paths from framework config). The base-path override is resolved
    // at the dispatch arm (flag > cih.toml > home) and passed through generically.
    let post_opts = cih_resolve::PostProcessOptions {
        route_base_path: cache.cxf_base_path.clone(),
    };
    resolvers.post_process(
        Some(&repo_root),
        &parse_output.parsed_files,
        &mut all_nodes,
        &mut edges,
        &post_opts,
    );

    // Apply user-defined resolve patterns (cih.patterns.toml) — teach CIH a repo's own framework
    // conventions (custom route annotations, …) without a hardcoded handler. Fail-soft: no file → no-op.
    let pattern_rules = cih_patterns::load_patterns(&repo_root);
    if !pattern_rules.is_empty() {
        let before = all_nodes.len();
        cih_resolve::apply_pattern_rules(&mut all_nodes, &mut edges, &pattern_rules);
        tracing::info!(
            route_rules = pattern_rules.routes.len(),
            synthesized_nodes = all_nodes.len() - before,
            "applied cih.patterns.toml resolve patterns"
        );
    }

    cih_resolve::propagate_loop_depths(&mut all_nodes, &edges);

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
        VersionId::new(version.clone()),
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
    crate::file_cache::FileHashIndex::from_map(current_hashes).save(&cih_dir)?;
    // Persist the config fingerprint alongside the file hashes so the next run's no-op gate can
    // detect a config-only change (e.g. a new --cxf-base-path or edited cih.patterns.toml).
    AnalyzeConfigState {
        fingerprint: analyze_config_fingerprint(&repo_root, &cache),
    }
    .save(&cih_dir)?;

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
        parsed_file_count: parse_output.parsed_files.len(),
        skipped_count: parse_output.skipped.len(),
        jar_node_count,
        jar_failed,
        reused_artifacts: false,
        cache_stats,
    })
}

#[derive(Clone, Default)]
pub struct AnalyzeCacheOptions {
    pub use_cache: bool,
    pub allow_noop: bool,
    /// Skip the integration + DI XML walk (faster on large repos).
    pub skip_xml_integration: bool,
    /// Explicit CXF servlet base path (e.g. `/rest`) for `<jaxrs:server>` routes.
    /// Resolved at the dispatch arm (flag > `cih.toml` > `~/.cih/config.toml`); `None`
    /// falls back to auto-detection.
    pub cxf_base_path: Option<String>,
}

/// Fingerprint of the analyze inputs that affect graph output but are **not** source-file
/// hashes: the resolved `cxf_base_path`, `skip_xml_integration`, and the effective
/// `cih.patterns.toml` rules. The incremental no-op reuse gate compares this against the value
/// stored from the last run so a config-only change (e.g. a new `--cxf-base-path`) re-runs the
/// resolve/post-process/emit path instead of silently reusing stale artifacts. Cache-control
/// fields (`use_cache`/`allow_noop`) are intentionally excluded — they don't change output.
pub(super) fn analyze_config_fingerprint(repo_root: &Path, cache: &AnalyzeCacheOptions) -> String {
    analyze_config_fingerprint_with(repo_root, cache, cih_lang::PARSE_CACHE_SCHEMA)
}

/// Schema-parameterized inner so tests can prove a bump changes the fingerprint
/// without bumping the real const.
fn analyze_config_fingerprint_with(
    repo_root: &Path,
    cache: &AnalyzeCacheOptions,
    parse_schema: u32,
) -> String {
    let patterns = cih_patterns::load_patterns(repo_root);
    let material = format!(
        "cxf_base_path={:?}\nskip_xml_integration={}\nparse_cache_schema={}\npatterns=\n{}",
        cache.cxf_base_path,
        cache.skip_xml_integration,
        parse_schema,
        cih_patterns::to_toml(&patterns),
    );
    blake3::hash(material.as_bytes()).to_hex()[..16].to_string()
}

/// The analyze config fingerprint persisted beside `file-hashes.json`, so the next run's no-op
/// gate can detect a config-only change. Modeled on [`crate::file_cache::FileHashIndex`].
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct AnalyzeConfigState {
    pub fingerprint: String,
}

impl AnalyzeConfigState {
    const FILE: &'static str = "analyze-config.json";

    pub(super) fn load(cih_dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(cih_dir.join(Self::FILE)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub(super) fn save(&self, cih_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(cih_dir)
            .with_context(|| format!("failed to create {}", cih_dir.display()))?;
        let path = cih_dir.join(Self::FILE);
        std::fs::write(&path, serde_json::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
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
    pub(super) fn with_version(mut self, version: String) -> Self {
        self.version = Some(version);
        self
    }
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
    pub falkor_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falkor_nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falkor_edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub falkor_error: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_varies_with_parse_schema() {
        let dir = std::env::temp_dir().join(format!("cih-fp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: true,
            skip_xml_integration: false,
            cxf_base_path: None,
        };

        let v1 = analyze_config_fingerprint_with(&dir, &cache, 1);
        let v2 = analyze_config_fingerprint_with(&dir, &cache, 2);
        std::fs::remove_dir_all(&dir).ok();

        assert_ne!(v1, v2, "a schema bump must invalidate the no-op gate");
        assert_eq!(v1, analyze_config_fingerprint_with(&dir, &cache, 1));
    }
}
