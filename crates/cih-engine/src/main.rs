mod scan;
mod scope;

use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use cih_core::{Edge, GraphArtifacts, Node, ParsedFile, VersionId};
use scope::ScopeFile;
use cih_falkor::FalkorStore;
use cih_graph_store::{GraphStore, LoadStats};
use clap::{Parser, Subcommand};
use scope::ScopeRequest;
use serde::Serialize;

/// Default FalkorDB URL (Homebrew redis squats 6379, FalkorDB on 6380).
const DEFAULT_FALKOR_URL: &str = "redis://127.0.0.1:6380";
const DEFAULT_GRAPH_KEY: &str = "cih";

#[derive(Debug, Parser)]
#[command(name = "cih-engine", about = "Code Intelligence Hub engine CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fast repository discovery pass. Writes .cih/repo-map.json.
    Scan {
        /// Repository root to scan.
        repo: PathBuf,
        /// Print RepoMap JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Parse selected files, emit structure graph, and load into FalkorDB.
    Analyze {
        /// Repository root to analyze.
        repo: PathBuf,
        /// Select all Java files, excluding decompiled dirs unless requested.
        #[arg(long)]
        all: bool,
        /// Select one or more module names, comma-delimited or repeated.
        #[arg(long = "module", value_delimiter = ',')]
        modules: Vec<String>,
        /// Include Java files matching this repo-relative glob. Can be repeated.
        #[arg(long)]
        include: Vec<String>,
        /// Exclude Java files matching this repo-relative glob. Can be repeated.
        #[arg(long)]
        exclude: Vec<String>,
        /// Include files under decompiled dirs such as .workspace-dependencies.
        #[arg(long)]
        include_decompiled: bool,
        /// Scope TOML file. Defaults to <repo>/cih.scope.toml when present.
        #[arg(long)]
        scope: Option<PathBuf>,
        /// Print the resolved ScopeFile JSON instead of the human summary.
        #[arg(long)]
        json: bool,
        /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
        #[arg(long, env = "CIH_GRAPH_KEY")]
        graph_key: Option<String>,
        /// Skip the FalkorDB load step (emit JSONL artifacts only).
        #[arg(long)]
        no_load: bool,
    },
    /// Re-run the resolve pass using the saved scope (.cih/scope.json), without re-scanning.
    /// Useful when the resolver changes but the source files have not.
    Resolve {
        /// Repository root (must contain .cih/scope.json from a prior `analyze` run).
        repo: PathBuf,
        /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
        #[arg(long, env = "CIH_GRAPH_KEY")]
        graph_key: Option<String>,
        /// Skip the FalkorDB load step (emit JSONL artifacts only).
        #[arg(long)]
        no_load: bool,
        /// Print the summary as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Scan { repo, json } => scan::run_scan(&repo, json),
        Command::Analyze {
            repo,
            all,
            modules,
            include,
            exclude,
            include_decompiled,
            scope,
            json,
            falkor_url,
            graph_key,
            no_load,
        } => run_analyze(
            repo,
            AnalyzeFlags {
                all,
                modules,
                include,
                exclude,
                include_decompiled,
                scope,
                json,
                falkor_url,
                graph_key,
                no_load,
            },
        ),
        Command::Resolve {
            repo,
            falkor_url,
            graph_key,
            no_load,
            json,
        } => run_resolve(repo, falkor_url, graph_key, no_load, json),
    }
}

#[derive(Debug)]
struct AnalyzeFlags {
    all: bool,
    modules: Vec<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    include_decompiled: bool,
    scope: Option<PathBuf>,
    json: bool,
    falkor_url: Option<String>,
    graph_key: Option<String>,
    no_load: bool,
}

fn run_analyze(repo: PathBuf, flags: AnalyzeFlags) -> Result<()> {
    // Scan + the no-scope-selected gate live here (they exit the process); the
    // DB-free emit core is `analyze_emit`, so it stays testable without FalkorDB.
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

    // bulk_load is MERGE/upsert — additive, NOT a full replace. Re-running the same
    // scope is idempotent, but narrowing scope leaves prior out-of-scope nodes in the
    // graph; pruning those is GraphDelta's job (Phase 4 / incremental re-index).
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

    // Exit non-zero when the load was attempted and failed, so automation/CI can tell
    // the graph was NOT updated (artifacts are still on disk for a retry).
    if matches!(load, LoadOutcome::Failed(_)) {
        process::exit(3);
    }
    Ok(())
}

/// Outcome of the FalkorDB load step — distinguishes a deliberate skip from a failure.
enum LoadOutcome {
    Loaded(LoadStats),
    Skipped,
    Failed(String),
}

impl LoadOutcome {
    fn status(&self) -> &'static str {
        match self {
            LoadOutcome::Loaded(_) => "loaded",
            LoadOutcome::Skipped => "skipped",
            LoadOutcome::Failed(_) => "failed",
        }
    }

    fn stats(&self) -> Option<&LoadStats> {
        match self {
            LoadOutcome::Loaded(stats) => Some(stats),
            _ => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            LoadOutcome::Failed(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// DB-free core of `analyze`: resolve scope → parse → write IR + GraphArtifacts.
/// Returns everything the caller needs to load and report. No process exits, no DB.
fn analyze_emit(scan: &scan::ScanResult, request: ScopeRequest) -> Result<EmitOutcome> {
    let scope_file = scope::resolve(&scan.repo_map, &scan.java_files, request)?;
    let scope_path = scope::write_scope_file(&scope_file)?;
    analyze_from_scope(scope_file, scope_path)
}

/// DB-free core shared by `analyze` and `resolve`: parse the files listed in
/// `scope_file` → resolve → write IR + GraphArtifacts.
fn analyze_from_scope(scope_file: ScopeFile, scope_path: PathBuf) -> Result<EmitOutcome> {
    let repo_root = PathBuf::from(&scope_file.repo_root);

    let parse_output = cih_parse::parse_files(&repo_root, &scope_file.files)?;
    let resolve_output = cih_resolve::resolve_edges(&parse_output.parsed_files);
    let edges = combined_edges(&parse_output.edges, &resolve_output.edges);

    // Version the graph by the CONTENT of the emitted nodes+edges plus parsed IR
    // (deterministic, already sorted) — not by the scope identity — so a changed
    // file body or resolver input yields a new version. Same content re-run →
    // same version.
    let version = content_version(&parse_output.nodes, &edges, &parse_output.parsed_files);

    let cih_dir = repo_root.join(".cih");
    let parsed_dir = cih_dir.join("parsed").join(&version);
    let parse_artifacts = cih_parse::write_parsed_files(&parsed_dir, &parse_output.parsed_files)?;

    let artifacts_dir = cih_dir.join("artifacts").join(&version);
    let artifacts = GraphArtifacts::write(
        &artifacts_dir,
        VersionId(version.clone()),
        &parse_output.nodes,
        &edges,
    )
    .with_context(|| {
        format!(
            "failed to write graph artifacts to {}",
            artifacts_dir.display()
        )
    })?;

    // Old version dirs are re-derivable intermediates; keep only the current one.
    prune_other_versions(&cih_dir.join("parsed"), &version)?;
    prune_other_versions(&cih_dir.join("artifacts"), &version)?;

    tracing::info!(
        nodes = parse_output.nodes.len(),
        edges = edges.len(),
        resolved_edges = resolve_output.edges.len(),
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
        node_count: parse_output.nodes.len(),
        edge_count: edges.len(),
        resolved_edge_count: resolve_output.edges.len(),
        unresolved_reference_count: resolve_output.skipped,
        unresolved_external_fqcns: resolve_output.unresolved_external_fqcns,
        parsed_file_count: parse_output.parsed_files.len(),
        skipped_count: parse_output.skipped.len(),
    })
}

fn run_resolve(
    repo: PathBuf,
    falkor_url: Option<String>,
    graph_key: Option<String>,
    no_load: bool,
    json: bool,
) -> Result<()> {
    let scope_path = repo.join(".cih").join("scope.json");
    let scope_file: ScopeFile = {
        let raw = std::fs::read_to_string(&scope_path)
            .with_context(|| format!("no saved scope at {} — run `analyze` first", scope_path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("malformed scope file at {}", scope_path.display()))?
    };

    let emit = analyze_from_scope(scope_file, scope_path)?;

    let load = if no_load {
        tracing::info!("Skipping FalkorDB load (--no-load)");
        LoadOutcome::Skipped
    } else {
        let url = falkor_url.as_deref().unwrap_or(DEFAULT_FALKOR_URL);
        let key = graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
        match load_to_falkor(url, key, &emit.artifacts) {
            Ok(stats) => {
                tracing::info!(nodes = stats.nodes, edges = stats.edges, "FalkorDB resolve load complete");
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

/// Everything `analyze_emit` produced (DB-free), used to load + report.
struct EmitOutcome {
    scope_file: scope::ScopeFile,
    scope_path: PathBuf,
    artifacts: GraphArtifacts,
    parsed_files_path: PathBuf,
    artifacts_dir: PathBuf,
    version: String,
    node_count: usize,
    edge_count: usize,
    resolved_edge_count: usize,
    unresolved_reference_count: u64,
    unresolved_external_fqcns: Vec<String>,
    parsed_file_count: usize,
    skipped_count: usize,
}

impl EmitOutcome {
    fn summary<'a>(&'a self, load: &'a LoadOutcome) -> AnalyzeSummary<'a> {
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
            falkor_status: load.status(),
            falkor_nodes: load.stats().map(|s| s.nodes),
            falkor_edges: load.stats().map(|s| s.edges),
            falkor_error: load.error(),
        }
    }

    fn print_human(&self, load: &LoadOutcome) {
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

/// blake3 (first 16 hex) over deterministic nodes+edges+IR → graph version.
fn content_version(nodes: &[Node], edges: &[Edge], parsed_files: &[ParsedFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    for node in nodes {
        hasher.update(&serde_json::to_vec(node).unwrap_or_default());
        hasher.update(b"\n");
    }
    for edge in edges {
        hasher.update(&serde_json::to_vec(edge).unwrap_or_default());
        hasher.update(b"\n");
    }
    for parsed in parsed_files {
        hasher.update(&serde_json::to_vec(parsed).unwrap_or_default());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex()[..16].to_string()
}

fn combined_edges(structure: &[Edge], resolved: &[Edge]) -> Vec<Edge> {
    // Dedup on (src, dst, kind); when two sources emit the same relationship keep the
    // higher-confidence edge. BTreeMap gives a deterministic iteration order.
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

/// Remove every direct child dir of `parent` except `keep`. Best-effort: failures to
/// remove a stale dir are logged, not fatal.
fn prune_other_versions(parent: &Path, keep: &str) -> Result<()> {
    if !parent.exists() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(parent).with_context(|| format!("failed to read {}", parent.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && entry.file_name().to_str() != Some(keep) {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), error = %err, "Failed to prune stale version dir");
            } else {
                tracing::debug!(path = %path.display(), "Pruned stale version dir");
            }
        }
    }
    Ok(())
}

/// Run the async FalkorDB bulk_load inside a short-lived tokio runtime.
/// The engine CLI is otherwise synchronous (rayon for parse, blocking I/O for
/// scan), so we spin up a minimal runtime only for the DB call.
fn load_to_falkor(
    url: &str,
    graph_key: &str,
    artifacts: &GraphArtifacts,
) -> Result<cih_graph_store::LoadStats> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    rt.block_on(async {
        let store = FalkorStore::connect(url, graph_key)
            .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
        store
            .ensure_schema()
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB ensure_schema: {e}"))?;
        let stats = store
            .bulk_load(artifacts)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB bulk_load: {e}"))?;
        Ok(stats)
    })
}

#[derive(Serialize)]
struct AnalyzeSummary<'a> {
    scope: &'a scope::ScopeFile,
    version: &'a str,
    scope_path: String,
    parsed_files_path: String,
    artifacts_path: String,
    node_count: usize,
    edge_count: usize,
    resolved_edge_count: usize,
    unresolved_reference_count: u64,
    unresolved_external_fqcns: &'a [String],
    parsed_file_count: usize,
    skipped_count: usize,
    /// "loaded" | "skipped" | "failed"
    falkor_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    falkor_nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    falkor_edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    falkor_error: Option<&'a str>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_repo() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("cih-emit-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(root.join("src/main/java/com/example")).unwrap();
        write(
            &root,
            "pom.xml",
            "<project><groupId>com.example</groupId><artifactId>demo</artifactId></project>",
        );
        write(
            &root,
            "src/main/java/com/example/OwnerService.java",
            "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() {}\n}\n",
        );
        write(
            &root,
            "src/main/java/com/example/OwnerController.java",
            "package com.example;\nclass OwnerController {\n  private OwnerService service;\n  public void handle() { service.findAll(); }\n}\n",
        );
        root
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn all_scope() -> ScopeRequest {
        ScopeRequest {
            all: true,
            ..ScopeRequest::default()
        }
    }

    #[test]
    fn analyze_emit_writes_artifacts_without_a_database() {
        let root = temp_repo();
        let scan = scan::scan_repo(&root).unwrap();
        let emit = analyze_emit(&scan, all_scope()).unwrap();

        // Structure was emitted and the JSONL artifacts exist on disk.
        assert!(emit.node_count > 0 && emit.edge_count > 0);
        assert_eq!(emit.skipped_count, 0);
        let nodes_jsonl = emit.artifacts_dir.join("nodes.jsonl");
        let edges_jsonl = emit.artifacts_dir.join("edges.jsonl");
        assert!(nodes_jsonl.exists(), "nodes.jsonl should exist");
        assert!(edges_jsonl.exists(), "edges.jsonl should exist");
        assert_eq!(
            fs::read_to_string(&nodes_jsonl).unwrap().lines().count(),
            emit.node_count
        );
        assert!(emit.resolved_edge_count > 0);
        let edges = fs::read_to_string(&edges_jsonl).unwrap();
        assert!(
            edges.contains("\"kind\":\"Calls\"")
                && edges.contains("Method:com.example.OwnerController#handle/0")
                && edges.contains("Method:com.example.OwnerService#findAll/0"),
            "resolved CALLS edge should be written"
        );

        // Skipped (no DB) maps to the right summary status, no exit.
        let summary = emit.summary(&LoadOutcome::Skipped);
        assert_eq!(summary.falkor_status, "skipped");
        assert!(summary.falkor_error.is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn content_version_is_stable_for_identical_content() {
        let root = temp_repo();
        let first = {
            let scan = scan::scan_repo(&root).unwrap();
            analyze_emit(&scan, all_scope()).unwrap().version
        };
        let second = {
            let scan = scan::scan_repo(&root).unwrap();
            analyze_emit(&scan, all_scope()).unwrap().version
        };
        assert_eq!(first, second, "same content must yield the same version");

        // Changing a file body changes the version + relocates the artifacts dir.
        write(
            &root,
            "src/main/java/com/example/OwnerService.java",
            "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() {}\n  public void findOne() {}\n}\n",
        );
        let scan = scan::scan_repo(&root).unwrap();
        let changed = analyze_emit(&scan, all_scope()).unwrap();
        assert_ne!(
            changed.version, first,
            "changed content must yield a new version"
        );

        // Prune keeps only the current version dir.
        let artifacts_parent = root.join(".cih").join("artifacts");
        let dirs: Vec<String> = fs::read_dir(&artifacts_parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(dirs, vec![changed.version.clone()]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ir_only_body_change_bumps_version() {
        // Verifies that a method-body edit (new call site, no new declarations) changes
        // the content_version, proving post-resolve versioning covers the IR.
        let root = temp_repo();
        let scan = scan::scan_repo(&root).unwrap();
        let v1 = analyze_emit(&scan, all_scope()).unwrap().version;

        // Replace handle() body with a different call — same method signature, new reference.
        write(
            &root,
            "src/main/java/com/example/OwnerController.java",
            "package com.example;\nclass OwnerController {\n  private OwnerService service;\n  public void handle() { service.findAll(); service.findAll(); }\n}\n",
        );
        let scan2 = scan::scan_repo(&root).unwrap();
        let v2 = analyze_emit(&scan2, all_scope()).unwrap().version;
        assert_ne!(v1, v2, "adding a call in a method body must bump the version");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_subcommand_reads_saved_scope() {
        let root = temp_repo();
        // First run analyze to produce .cih/scope.json.
        let scan = scan::scan_repo(&root).unwrap();
        let v1 = analyze_emit(&scan, all_scope()).unwrap().version;

        // resolve subcommand reads scope.json and re-runs — same content → same version.
        let scope_path = root.join(".cih").join("scope.json");
        let raw = fs::read_to_string(&scope_path).unwrap();
        let scope_file: ScopeFile = serde_json::from_str(&raw).unwrap();
        let v2 = analyze_from_scope(scope_file, scope_path).unwrap().version;
        assert_eq!(v1, v2, "resolve with same scope must produce the same version");

        fs::remove_dir_all(&root).unwrap();
    }
}
