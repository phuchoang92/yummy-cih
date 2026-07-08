//! `cih-engine taint` — Phase 0 + Phase 1 + Phase 2 + Phase 3 taint analysis.
//!
//! Phase 0: BFS on the method-granularity call graph → finds inter-procedural taint paths.
//! Phase 1: intra-procedural variable liveness for source methods → confirms/penalises paths.
//! Phase 2: on-demand CFG construction + dominance tree for Phase 1-confirmed source methods.
//! Phase 3: PDG-based flow-sensitive, kill-aware taint (reaching defs → DataDep/ControlDep).

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{GraphArtifacts, Node, NodeId};
use cih_taint::{find_taint_paths, TaintPath};

use crate::db::{load_to_falkor, LoadOutcome};
use crate::versioning::latest_graph_artifacts;
use crate::{DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

pub struct TaintFlags {
    pub falkor_url: Option<String>,
    pub graph_key: Option<String>,
    pub no_load: bool,
    /// Run intra-procedural liveness analysis (Phase 1). Default: true.
    pub intra_proc: bool,
    /// Run CFG construction + dominance tree (Phase 2). Default: true.
    pub cfg: bool,
    /// Run PDG-based flow-sensitive taint (Phase 3). Default: true.
    pub pdg: bool,
    pub json: bool,
}

#[derive(Default)]
struct CfgStats {
    methods_analyzed: usize,
    total_blocks: usize,
    total_edges: usize,
    max_cyclomatic: usize,
    dominated_pairs: usize,
    ir_unavailable: usize,
}

#[derive(Default)]
struct PdgStats {
    methods_analyzed: usize,
    confirmed_sinks: usize,
    conditional_sinks: usize,
    ir_unavailable: usize,
}

pub fn run_taint(repo: PathBuf, flags: TaintFlags) -> Result<()> {
    let span = tracing::info_span!("taint", repo = %repo.display());
    let _enter = span.enter();

    let cih_dir = repo.join(".cih");

    // ── Load latest graph artifacts ───────────────────────────────────────────
    let artifacts = latest_graph_artifacts(&repo)
        .context("no graph artifacts found — run `analyze` first")?;

    tracing::info!(version = %artifacts.version, "loaded graph artifacts");

    let nodes = artifacts.read_nodes().context("failed to read nodes.jsonl")?;
    let edges = artifacts.read_edges().context("failed to read edges.jsonl")?;

    tracing::info!(nodes = nodes.len(), edges = edges.len(), "graph loaded");

    // ── Run Phase 0 ───────────────────────────────────────────────────────────
    let mut ui = crate::ui::PhaseProgress::new();
    ui.spin("Phase 0: inter-procedural taint BFS");

    let rules = cih_taint::load_taint_rules(&repo);
    let mut paths = find_taint_paths(&nodes, &edges, &rules);

    ui.finish_with(format!(
        "{} taint paths found (Phase 0)",
        crate::ui::fmt_count(paths.len())
    ));

    // Derive intra-proc sink name patterns from the loaded rules so Phase 1 and Phase 3
    // always use the same set (which now includes any user additions from cih.taint.toml).
    let sink_name_patterns_owned: Vec<String> = rules.extra_sink_name_patterns.clone();
    let sink_name_patterns: Vec<&str> =
        sink_name_patterns_owned.iter().map(|s| s.as_str()).collect();
    let node_map: FxHashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();
    let repo_ref = repo.as_path();
    // Snapshot Phase-0 scores before Phase 1 modifies them.
    // Phase 3 applies its multiplier against this baseline so that a Phase-1 penalty
    // cannot be silently erased by a Phase-3 boost (or vice versa).
    let original_confidence: Vec<f32> = paths.iter().map(|p| p.confidence).collect();

    // ── Run Phase 1 (optional) ────────────────────────────────────────────────
    if flags.intra_proc && !paths.is_empty() {
        ui.spin("Phase 1: intra-procedural IR refinement");

        let refinements = cih_taint::liveness::refine_paths(
            &paths,
            &|id| node_map.get(id).map(|n| n.file.clone()),
            |file| std::fs::read_to_string(repo_ref.join(file)).ok(),
            &sink_name_patterns,
        );

        // Apply confidence multipliers.
        let confirmed_count = refinements.iter().filter(|r| r.intra_confirmed).count();
        let unavail_count = refinements.iter().filter(|r| r.ir_unavailable).count();
        for r in &refinements {
            if let Some(p) = paths.get_mut(r.path_index) {
                p.confidence = (p.confidence * r.confidence_multiplier).clamp(0.0, 1.0);
            }
        }
        ui.finish_with(format!(
            "{} confirmed, {} IR unavailable",
            crate::ui::fmt_count(confirmed_count),
            crate::ui::fmt_count(unavail_count),
        ));
    }

    // ── Run Phase 2 (optional) ────────────────────────────────────────────────
    let mut cfg_stats = CfgStats::default();
    if flags.intra_proc && flags.cfg && !paths.is_empty() {
        ui.spin("Phase 2: CFG construction + dominance tree");

        // Build CFG for each unique source method on a taint path.
        let unique_sources: FxHashSet<&NodeId> =
            paths.iter().map(|p| &p.source).collect();

        for source_id in &unique_sources {
            let Some(node) = node_map.get(source_id) else { continue };
            let Ok(src) = std::fs::read_to_string(repo_ref.join(&node.file)) else { continue };
            let Some(cfg) = cih_taint::build_cfg(source_id, &src) else {
                cfg_stats.ir_unavailable += 1;
                continue;
            };
            let dom = cfg.compute_dominators();
            cfg_stats.methods_analyzed += 1;
            cfg_stats.total_blocks += cfg.block_count();
            cfg_stats.total_edges += cfg.edge_count();
            cfg_stats.max_cyclomatic =
                cfg_stats.max_cyclomatic.max(cfg.cyclomatic_complexity());
            cfg_stats.dominated_pairs += dom.dominated_ids().count();
            tracing::debug!(
                method = %source_id.as_str(),
                blocks = cfg.block_count(),
                edges = cfg.edge_count(),
                cc = cfg.cyclomatic_complexity(),
                "CFG built"
            );
        }

        ui.finish_with(format!(
            "{} CFGs built, max CC={}",
            crate::ui::fmt_count(cfg_stats.methods_analyzed),
            cfg_stats.max_cyclomatic,
        ));
    }

    // ── Run Phase 3 (optional) ────────────────────────────────────────────────
    let mut pdg_stats = PdgStats::default();
    if flags.intra_proc && flags.cfg && flags.pdg && !paths.is_empty() {
        ui.spin("Phase 3: PDG construction + flow-sensitive taint");

        // Sanitizer node-id patterns from the same rules used by Phase 0.
        let sanitizer_patterns: Vec<&str> =
            rules.sanitizers.iter().map(|s| s.node_id_pattern.as_str()).collect();

        let refinements = cih_taint::flow_sensitive::refine_paths(
            &paths,
            &|id| node_map.get(id).map(|n| n.file.clone()),
            |file| std::fs::read_to_string(repo_ref.join(file)).ok(),
            &sink_name_patterns,
            &sanitizer_patterns,
        );

        for r in &refinements {
            // "IR unavailable" means the source file could not be read or parsed.
            // Detect it via the pdg_clean/confirmed/conditional fields rather than a
            // float equality check on confidence_multiplier (which would misfire if a
            // future neutral multiplier of 1.0 is ever added).
            if !r.pdg_confirmed && !r.pdg_conditional && !r.pdg_clean {
                pdg_stats.ir_unavailable += 1;
                continue;
            }
            pdg_stats.methods_analyzed += 1;
            if r.pdg_confirmed {
                pdg_stats.confirmed_sinks += 1;
            }
            if r.pdg_conditional {
                pdg_stats.conditional_sinks += 1;
            }
            // Apply Phase 3 against the Phase-0 baseline, not the Phase-1-modified score,
            // so the two phases score independently rather than compounding.
            if let Some(p) = paths.get_mut(r.path_index) {
                p.confidence =
                    (original_confidence[r.path_index] * r.confidence_multiplier).clamp(0.0, 1.0);
            }
        }

        ui.finish_with(format!(
            "{} PDG confirmed, {} conditional",
            crate::ui::fmt_count(pdg_stats.confirmed_sinks),
            crate::ui::fmt_count(pdg_stats.conditional_sinks),
        ));
    }

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
        .join(artifacts.version.as_str());

    ui.spin("Writing taint artifacts");
    let taint_artifacts = GraphArtifacts::write(
        &taint_dir,
        artifacts.version.clone(),
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
        print_human_report(&paths, &load, &taint_dir, &cfg_stats, &pdg_stats);
    }

    if matches!(load, LoadOutcome::Failed(_)) {
        std::process::exit(3);
    }
    Ok(())
}

fn print_human_report(paths: &[TaintPath], load: &LoadOutcome, taint_dir: &Path, cfg: &CfgStats, pdg: &PdgStats) {
    crate::ui::print_header("Taint", "Phase 0 + Phase 1 + Phase 2 + Phase 3", None);
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

    if cfg.methods_analyzed > 0 {
        crate::ui::print_row("CFGs built", &crate::ui::fmt_count(cfg.methods_analyzed));
        crate::ui::print_row("  Total blocks", &crate::ui::fmt_count(cfg.total_blocks));
        crate::ui::print_row("  Total edges", &crate::ui::fmt_count(cfg.total_edges));
        crate::ui::print_row("  Max CC", &cfg.max_cyclomatic.to_string());
    }
    if cfg.ir_unavailable > 0 {
        crate::ui::print_row("CFG unavailable", &crate::ui::fmt_count(cfg.ir_unavailable));
    }
    if pdg.methods_analyzed > 0 {
        crate::ui::print_row("PDG confirmed", &crate::ui::fmt_count(pdg.confirmed_sinks));
        crate::ui::print_row("PDG conditional", &crate::ui::fmt_count(pdg.conditional_sinks));
    }
    if pdg.ir_unavailable > 0 {
        crate::ui::print_row("PDG unavailable", &crate::ui::fmt_count(pdg.ir_unavailable));
    }
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
    println!("  \x1b[2mPhase 0: inter-proc BFS  Phase 1: intra-proc IR  Phase 2: CFG + dom-tree  Phase 3: PDG taint");
    println!("  Use --no-intra-proc / --no-cfg / --no-pdg to skip phases, or --json for machine output.\x1b[0m");
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
