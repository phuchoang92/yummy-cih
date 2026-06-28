//! Facade for running all taint analysis phases in sequence.
//!
//! [`run_taint_analysis`] is the primary entry point. It handles phase ordering,
//! caches CFGs between Phase 2 and Phase 3 to avoid rebuilding them, and keeps
//! Phase 1 and Phase 3 confidence adjustments independent of each other.

use std::collections::{HashMap, HashSet};

use cih_core::{Edge, Node, NodeId};

use crate::error::TaintResult;
use crate::interproc::TaintPath;
use crate::rules::TaintRules;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Identifies a taint analysis pass, used for logging and result metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaintPass {
    /// Phase 0: inter-procedural BFS on the call graph.
    InterProc,
    /// Phase 1: flow-insensitive intra-procedural variable liveness.
    IntraProc,
    /// Phase 2: CFG construction and Cooper-Harvey-Kennedy dominance tree.
    CfgBuild,
    /// Phase 3: PDG-based flow-sensitive, kill-aware taint.
    FlowSensitive,
}

/// Controls which analysis phases are executed.
pub struct TaintPhaseConfig {
    /// Phase 1: intra-procedural liveness refinement. Default: `true`.
    pub run_intra_proc: bool,
    /// Phase 2: CFG construction + dominance tree. Default: `true`.
    pub run_cfg: bool,
    /// Phase 3: PDG + flow-sensitive taint. Default: `true`. Requires `run_cfg`.
    pub run_pdg: bool,
}

impl Default for TaintPhaseConfig {
    fn default() -> Self {
        Self { run_intra_proc: true, run_cfg: true, run_pdg: true }
    }
}

// ── Result types ──────────────────────────────────────────────────────────────

/// Statistics from Phase 2 (CFG construction).
#[derive(Default, Debug)]
pub struct TaintCfgStats {
    pub methods_analyzed: usize,
    pub total_blocks: usize,
    pub total_edges: usize,
    pub max_cyclomatic: usize,
    pub dominated_pairs: usize,
    pub ir_unavailable: usize,
}

/// Statistics from Phase 3 (PDG taint).
#[derive(Default, Debug)]
pub struct TaintPdgStats {
    pub methods_analyzed: usize,
    pub confirmed_sinks: usize,
    pub conditional_sinks: usize,
    pub ir_unavailable: usize,
}

/// Output of a complete taint analysis run.
pub struct TaintAnalysisResult {
    /// Taint paths found by Phase 0, with confidence adjusted by enabled phases.
    pub paths: Vec<TaintPath>,
    /// Aggregated CFG construction statistics (Phase 2).
    pub cfg_stats: TaintCfgStats,
    /// Aggregated PDG taint statistics (Phase 3).
    pub pdg_stats: TaintPdgStats,
}

// ── Input ─────────────────────────────────────────────────────────────────────

/// All inputs needed for a complete taint analysis run.
pub struct TaintAnalysisInput<'a> {
    /// Method and edge nodes from the graph artifact.
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
    /// Sink/sanitizer rules to use (combine `default_rules()` with any user overrides).
    pub rules: &'a TaintRules,
    /// Resolves a file path (relative to repo root) to its full source text.
    pub resolve_source: Box<dyn Fn(&str) -> Option<String> + 'a>,
    /// Resolves a method node ID to the relative file path where it is defined.
    pub node_file: Box<dyn Fn(&NodeId) -> Option<String> + 'a>,
    /// Which phases to run.
    pub phases: TaintPhaseConfig,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run all enabled taint analysis phases and return scored taint paths.
///
/// Phase ordering and confidence scoring:
///
/// 1. **Phase 0** (always runs): inter-procedural BFS produces initial `confidence` scores.
/// 2. **Phase 1** (optional): intra-proc liveness multiplier applied *in place* to paths.
/// 3. **Phase 2** (optional): CFGs built and cached per unique source method.
/// 4. **Phase 3** (optional, requires Phase 2): uses cached CFGs to build PDGs, applies
///    its confidence multiplier against the **Phase-0 baseline** — independently of Phase 1
///    — so neither refinement silently erases the other.
pub fn run_taint_analysis(input: TaintAnalysisInput<'_>) -> TaintResult<TaintAnalysisResult> {
    let TaintAnalysisInput { nodes, edges, rules, resolve_source, node_file, phases } = input;

    // ── Phase 0 ───────────────────────────────────────────────────────────────
    let mut paths = crate::interproc::find_taint_paths(nodes, edges, rules);

    let sink_name_patterns_owned: Vec<String> = rules.extra_sink_name_patterns.clone();
    let sink_patterns: Vec<&str> = sink_name_patterns_owned.iter().map(|s| s.as_str()).collect();

    // Snapshot Phase-0 baseline before Phase 1 modifies scores in place.
    let baseline_confidence: Vec<f32> = paths.iter().map(|p| p.confidence).collect();

    // ── Phase 1 ───────────────────────────────────────────────────────────────
    if phases.run_intra_proc && !paths.is_empty() {
        let refinements = crate::liveness::refine_paths(
            &paths,
            &*node_file,
            |file| resolve_source(file),
            &sink_patterns,
        );
        for r in &refinements {
            if let Some(p) = paths.get_mut(r.path_index) {
                p.confidence = (p.confidence * r.confidence_multiplier).clamp(0.0, 1.0);
            }
        }
        tracing::debug!(
            confirmed = refinements.iter().filter(|r| r.intra_confirmed).count(),
            unavail = refinements.iter().filter(|r| r.ir_unavailable).count(),
            "Phase 1 complete"
        );
    }

    // ── Phase 2: build and cache CFGs ─────────────────────────────────────────
    let mut cfg_stats = TaintCfgStats::default();
    let mut cfg_cache: HashMap<NodeId, crate::cfg::Cfg> = HashMap::new();

    if phases.run_cfg && !paths.is_empty() {
        let unique_sources: HashSet<&NodeId> = paths.iter().map(|p| &p.source).collect();

        for source_id in &unique_sources {
            let Some(file) = node_file(source_id) else { continue };
            let Some(src) = resolve_source(&file) else { continue };
            let Some(cfg) = crate::cfg::build_cfg(source_id, &src) else {
                cfg_stats.ir_unavailable += 1;
                continue;
            };

            let dom = cfg.compute_dominators();
            cfg_stats.methods_analyzed += 1;
            cfg_stats.total_blocks += cfg.block_count();
            cfg_stats.total_edges += cfg.edge_count();
            cfg_stats.max_cyclomatic = cfg_stats.max_cyclomatic.max(cfg.cyclomatic_complexity());
            cfg_stats.dominated_pairs += dom.dominated_ids().count();

            tracing::debug!(
                method = %source_id.as_str(),
                blocks = cfg.block_count(),
                edges = cfg.edge_count(),
                cc = cfg.cyclomatic_complexity(),
                "Phase 2: CFG built"
            );

            cfg_cache.insert((*source_id).clone(), cfg);
        }

        tracing::debug!(
            built = cfg_stats.methods_analyzed,
            unavail = cfg_stats.ir_unavailable,
            "Phase 2 complete"
        );
    }

    // ── Phase 3: PDG + flow-sensitive taint (reuses cached CFGs) ─────────────
    let mut pdg_stats = TaintPdgStats::default();

    if phases.run_cfg && phases.run_pdg && !paths.is_empty() {
        let sanitizer_patterns: Vec<&str> =
            rules.sanitizers.iter().map(|s| s.node_id_pattern.as_str()).collect();

        for (i, path) in paths.iter_mut().enumerate() {
            let Some(cfg) = cfg_cache.get(&path.source) else {
                pdg_stats.ir_unavailable += 1;
                continue;
            };

            let dom = cfg.compute_dominators();
            let reaching = crate::pdg::compute_reaching_defs(cfg, &cfg.param_names);
            let pdg = crate::pdg::build_pdg(cfg, Some(&dom), Some(&reaching));

            let result = crate::flow_sensitive::analyze_with_pdg(
                cfg,
                &pdg,
                &reaching,
                &cfg.param_names,
                &sink_patterns,
                &sanitizer_patterns,
            );

            let pdg_confirmed = !result.confirmed_sinks.is_empty();
            let pdg_conditional = !result.conditionally_tainted_sinks.is_empty();

            pdg_stats.methods_analyzed += 1;
            if pdg_confirmed {
                pdg_stats.confirmed_sinks += 1;
            }
            if pdg_conditional {
                pdg_stats.conditional_sinks += 1;
            }

            // Apply Phase 3 multiplier against the Phase-0 baseline so the two
            // refinement phases score independently — neither can silently amplify
            // or cancel out the other's effect.
            path.confidence =
                (baseline_confidence[i] * result.confidence_multiplier).clamp(0.0, 1.0);
        }

        tracing::debug!(
            confirmed = pdg_stats.confirmed_sinks,
            conditional = pdg_stats.conditional_sinks,
            unavail = pdg_stats.ir_unavailable,
            "Phase 3 complete"
        );
    }

    Ok(TaintAnalysisResult { paths, cfg_stats, pdg_stats })
}
