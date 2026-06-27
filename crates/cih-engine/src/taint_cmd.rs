//! `cih-engine taint` — Phase 0 inter-procedural taint analysis.
//!
//! Reads the latest graph artifacts for a repo, runs the BFS taint pass, writes
//! `TaintFlow` edges to `.cih/artifacts-taint/<version>/edges.jsonl`, and optionally
//! loads them into FalkorDB as an incremental append.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{GraphArtifacts, Node, VersionId};
use cih_taint::{default_rules, find_taint_paths, TaintPath};

use crate::db::{load_to_falkor, LoadOutcome};
use crate::versioning::latest_graph_artifacts;
use crate::{DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

pub struct TaintFlags {
    pub falkor_url: Option<String>,
    pub graph_key: Option<String>,
    pub no_load: bool,
    pub json: bool,
}

pub fn run_taint(repo: PathBuf, flags: TaintFlags) -> Result<()> {
    let span = tracing::info_span!("taint", repo = %repo.display());
    let _enter = span.enter();

    let cih_dir = repo.join(".cih");

    // ── Load latest graph artifacts ───────────────────────────────────────────
    let artifacts = latest_graph_artifacts(&repo)
        .context("no graph artifacts found — run `analyze` first")?;

    tracing::info!(version = %artifacts.version.0, "loaded graph artifacts");

    let nodes = artifacts.read_nodes().context("failed to read nodes.jsonl")?;
    let edges = artifacts.read_edges().context("failed to read edges.jsonl")?;

    tracing::info!(nodes = nodes.len(), edges = edges.len(), "graph loaded");

    // ── Run taint pass ────────────────────────────────────────────────────────
    let mut ui = crate::ui::PhaseProgress::new();
    ui.spin("Running taint analysis");

    let rules = default_rules();
    let paths = find_taint_paths(&nodes, &edges, &rules);

    ui.finish_with(format!(
        "{} taint paths found",
        crate::ui::fmt_count(paths.len())
    ));

    if paths.is_empty() {
        if flags.json {
            println!("[]");
        } else {
            println!("No taint paths found.");
        }
        return Ok(());
    }

    // ── Emit TaintFlow edges ──────────────────────────────────────────────────
    let taint_edges: Vec<_> = paths.iter().map(TaintPath::to_edge).collect();
    let empty_nodes: Vec<Node> = vec![];

    let taint_dir = cih_dir
        .join("artifacts-taint")
        .join(&artifacts.version.0);

    ui.spin("Writing taint artifacts");
    let taint_artifacts = GraphArtifacts::write(
        &taint_dir,
        VersionId(artifacts.version.0.clone()),
        &empty_nodes,
        &taint_edges,
    )
    .context("failed to write taint artifacts")?;
    ui.finish_with(format!(
        "{} TaintFlow edges written",
        crate::ui::fmt_count(taint_edges.len())
    ));

    // ── Load into FalkorDB ────────────────────────────────────────────────────
    let load = if flags.no_load {
        tracing::info!("Skipping FalkorDB load (--no-load)");
        LoadOutcome::Skipped
    } else {
        let url = flags.falkor_url.as_deref().unwrap_or(DEFAULT_FALKOR_URL);
        let key = flags.graph_key.as_deref().unwrap_or(DEFAULT_GRAPH_KEY);
        ui.spin("Loading into FalkorDB");
        match load_to_falkor(url, key, &taint_artifacts) {
            Ok(stats) => {
                tracing::info!(
                    edges = stats.edges,
                    url,
                    graph = key,
                    "TaintFlow edges loaded into FalkorDB"
                );
                ui.finish_with(format!(
                    "{} edges loaded",
                    crate::ui::fmt_count(stats.edges as usize)
                ));
                LoadOutcome::Loaded(stats)
            }
            Err(err) => {
                tracing::warn!(error = %err, "FalkorDB load failed — taint artifacts are on disk");
                ui.finish_with(format!("FalkorDB load failed: {err}"));
                LoadOutcome::Failed(format!("{err:#}"))
            }
        }
    };

    // ── Report ────────────────────────────────────────────────────────────────
    if flags.json {
        print_json_report(&paths, &load, &taint_artifacts)?;
    } else {
        print_human_report(&paths, &load, &taint_dir);
    }

    if matches!(load, LoadOutcome::Failed(_)) {
        std::process::exit(3);
    }
    Ok(())
}

fn print_human_report(paths: &[TaintPath], load: &LoadOutcome, taint_dir: &Path) {
    crate::ui::print_header("Taint", "Phase 0", None);
    crate::ui::print_row("Paths", &crate::ui::fmt_count(paths.len()));

    // Count by category.
    let sql = paths.iter().filter(|p| p.category == cih_taint::SinkCategory::Sql).count();
    let exec = paths.iter().filter(|p| p.category == cih_taint::SinkCategory::Exec).count();
    let file = paths.iter().filter(|p| p.category == cih_taint::SinkCategory::File).count();
    let html = paths.iter().filter(|p| p.category == cih_taint::SinkCategory::Html).count();

    if sql > 0 { crate::ui::print_row("  SQL", &crate::ui::fmt_count(sql)); }
    if exec > 0 { crate::ui::print_row("  Exec", &crate::ui::fmt_count(exec)); }
    if file > 0 { crate::ui::print_row("  File", &crate::ui::fmt_count(file)); }
    if html > 0 { crate::ui::print_row("  HTML", &crate::ui::fmt_count(html)); }

    crate::ui::print_row("Artifacts", &taint_dir.display().to_string());

    let falkor_str = match load {
        LoadOutcome::Loaded(s) => format!("{} edges", crate::ui::fmt_count(s.edges as usize)),
        LoadOutcome::Skipped => "\x1b[2mskipped (--no-load)\x1b[0m".to_string(),
        LoadOutcome::Reused => "\x1b[2mreused\x1b[0m".to_string(),
        LoadOutcome::Failed(e) => format!("\x1b[31mfailed\x1b[0m  \x1b[2m{e}\x1b[0m"),
    };
    crate::ui::print_row("FalkorDB", &falkor_str);
    eprintln!();

    // Print top 20 paths.
    let show = paths.len().min(20);
    println!("Top {show} taint paths (sorted by confidence):");
    println!();

    let mut sorted: Vec<&TaintPath> = paths.iter().collect();
    sorted.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));

    for (i, path) in sorted.iter().take(show).enumerate() {
        let source_short = short_name(path.source.as_str());
        let sink_short = short_name(path.sink_method.as_str());
        println!(
            "  {:>2}. [{:.2}] {:?} — {} → {} ({} hops)",
            i + 1,
            path.confidence,
            path.category,
            source_short,
            sink_short,
            path.edge_count(),
        );
    }

    if paths.len() > 20 {
        println!("  … and {} more", paths.len() - 20);
    }
    println!();
    println!("  \x1b[2mNote: Phase 0 taint has no argument tracking — expect false positives.");
    println!("  Run `cih-engine taint --no-load --json` for machine-readable output.\x1b[0m");
}

fn print_json_report(
    paths: &[TaintPath],
    _load: &LoadOutcome,
    taint_artifacts: &GraphArtifacts,
) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Report<'a> {
        path_count: usize,
        artifacts_path: String,
        paths: &'a [TaintPath],
    }
    let report = Report {
        path_count: paths.len(),
        artifacts_path: taint_artifacts
            .edges_path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        paths,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Shorten a fully-qualified method node ID to `ClassName#method`.
fn short_name(node_id: &str) -> String {
    // Format: "Method:com.example.pkg.ClassName#method/arity"
    let without_prefix = node_id.split(':').nth(1).unwrap_or(node_id);
    // Take the last segment of the FQCN + method name.
    if let Some(hash_pos) = without_prefix.rfind('#') {
        let class = without_prefix[..hash_pos]
            .rsplit('.')
            .next()
            .unwrap_or(&without_prefix[..hash_pos]);
        let method = &without_prefix[hash_pos + 1..];
        // Strip arity suffix "/N"
        let method = method.rfind('/').map(|i| &method[..i]).unwrap_or(method);
        format!("{class}#{method}")
    } else {
        without_prefix.to_string()
    }
}
