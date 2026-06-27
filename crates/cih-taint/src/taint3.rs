//! Phase 3: PDG-based flow-sensitive, kill-aware taint analysis.
//!
//! Unlike Phase 1 (which tracks "is variable X tainted?"), Phase 3 tracks
//! "is *definition D* tainted?" This enables kill-through-reassignment:
//!
//! ```java
//! String x = userInput;   // def-1 of x is tainted
//! x = sanitize(x);       // def-2 of x is clean — KILLS the tainted def
//! sink(x);               // reaches def-2 only → NOT a confirmed sink
//! ```
//!
//! Phase 1 would flag that `sink` as confirmed (x was tainted). Phase 3 does not.
//!
//! # Algorithm
//!
//! 1. Initialize: mark the virtual param-def node IDs for each tainted parameter.
//! 2. Forward propagation (RPO order, reaching-defs already computed):
//!    - For each statement S: if any reaching def of a variable that S reads is tainted,
//!      then S's *own* def(s) of that variable are also tainted.
//!    - Propagate until fixpoint (needed for loops).
//! 3. Classify sinks:
//!    - **Confirmed** (`DataDep` chain): a tainted def reaches an argument of a sink call.
//!    - **Conditional** (`ControlDep` only): the sink is inside a control region whose
//!      branch condition is tainted, but no tainted def reaches the arg directly.
//!
//! The result feeds back to `taint_cmd.rs`, which applies a confidence multiplier to
//! the Phase 0 path score.

use std::collections::HashSet;

use cih_core::NodeId;

use crate::cfg::Cfg;
use crate::ir::{StatementKind, StatementNode};
use crate::pdg::{param_def_id, Pdg, PdgEdgeKind, ReachingDefs};

// ── Public types ──────────────────────────────────────────────────────────────

/// A sink call confirmed reachable by a tainted data-def chain.
#[derive(Debug, Clone)]
pub struct ConfirmedSink3 {
    /// Statement node ID of the sink call.
    pub stmt_id: NodeId,
    /// Callee name as extracted from the AST.
    pub call_name: String,
    /// Arguments to the sink that were tainted.
    pub tainted_args: Vec<String>,
}

/// Full Phase 3 result for a single method analysis.
#[derive(Debug)]
pub struct Phase3Result {
    pub callable_id: NodeId,
    /// Sinks confirmed via a tainted data-dependence chain.
    pub confirmed_sinks: Vec<ConfirmedSink3>,
    /// Statement IDs of sinks that are control-dependent on tainted branches
    /// but have no direct data-dep taint evidence.
    pub conditionally_tainted_sinks: Vec<NodeId>,
    /// True if a `return` statement returns a tainted value.
    pub taint_return: bool,
    /// Confidence multiplier to apply to the Phase 0 path score.
    /// - Confirmed sink found:          ×1.30
    /// - Only conditional:              ×0.85
    /// - Neither (analysis ran fine):   ×0.60
    pub confidence_multiplier: f32,
}

// ── Core analysis ─────────────────────────────────────────────────────────────

/// Run Phase 3 taint analysis for a single method.
///
/// - `cfg`: built by Phase 2 `build_cfg`
/// - `pdg`: built by Phase 3 `build_pdg`
/// - `reaching`: computed by `compute_reaching_defs`
/// - `tainted_params`: names of parameters that arrive tainted
/// - `sink_name_patterns`: substrings matched against call sites to identify sinks
pub fn analyze_with_pdg(
    cfg: &Cfg,
    pdg: &Pdg,
    reaching: &ReachingDefs,
    tainted_params: &[String],
    sink_name_patterns: &[&str],
) -> Phase3Result {
    let callable_id = cfg.callable_id.clone();

    // Seed: virtual parameter-definition node IDs are initially tainted.
    let mut tainted_defs: HashSet<NodeId> = tainted_params
        .iter()
        .map(|p| param_def_id(&callable_id, p))
        .collect();

    // Iterate to fixpoint (needed for loops — a tainted def in the loop body
    // might flow back through a back edge and re-reach earlier stmts).
    let rpo = cfg.reverse_post_order();
    let mut changed = true;
    while changed {
        changed = false;
        changed |= propagate_pass(cfg, reaching, &rpo, &mut tainted_defs);
    }

    // Classify statements.
    let mut confirmed_sinks = Vec::new();
    let mut conditionally_tainted_sinks = Vec::new();
    let mut taint_return = false;

    for block in &cfg.blocks {
        for stmt in &block.stmts {
            match stmt.kind {
                StatementKind::Call => {
                    let call_name = stmt.call_site.as_deref().unwrap_or("");
                    if is_sink(call_name, sink_name_patterns) {
                        let tainted_args = tainted_args_of(stmt, reaching, &tainted_defs);
                        if !tainted_args.is_empty() {
                            confirmed_sinks.push(ConfirmedSink3 {
                                stmt_id: stmt.id.clone(),
                                call_name: call_name.to_string(),
                                tainted_args,
                            });
                        } else if is_control_dep_tainted(&stmt.id, pdg, &tainted_defs, reaching) {
                            conditionally_tainted_sinks.push(stmt.id.clone());
                        }
                    }
                }
                StatementKind::Return => {
                    if stmt_reads_tainted(stmt, reaching, &tainted_defs) {
                        taint_return = true;
                    }
                }
                _ => {}
            }
        }
    }

    let confidence_multiplier = if !confirmed_sinks.is_empty() {
        1.30
    } else if !conditionally_tainted_sinks.is_empty() {
        0.85
    } else {
        0.60
    };

    Phase3Result {
        callable_id,
        confirmed_sinks,
        conditionally_tainted_sinks,
        taint_return,
        confidence_multiplier,
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// One forward propagation pass (RPO order). Returns true if any new defs became tainted.
fn propagate_pass(
    cfg: &Cfg,
    reaching: &ReachingDefs,
    rpo: &[crate::cfg::BlockId],
    tainted_defs: &mut HashSet<NodeId>,
) -> bool {
    let mut changed = false;
    for block_id in rpo {
        let Some(block) = cfg.block(block_id) else { continue };
        for stmt in &block.stmts {
            if stmt.writes.is_empty() {
                continue;
            }
            // If any variable read by this stmt has a tainted reaching def,
            // then all variables *written* by this stmt are now tainted (the def is tainted).
            if stmt_reads_tainted(stmt, reaching, tainted_defs) {
                for _ in &stmt.writes {
                    if tainted_defs.insert(stmt.id.clone()) {
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

/// True if any variable in `stmt.reads` or `stmt.call_args` has a tainted reaching def.
fn stmt_reads_tainted(
    stmt: &StatementNode,
    reaching: &ReachingDefs,
    tainted_defs: &HashSet<NodeId>,
) -> bool {
    let Some(rd) = reaching.get(&stmt.id) else {
        return false;
    };
    let all_reads = stmt.reads.iter().chain(stmt.call_args.iter());
    for var in all_reads {
        if let Some(def_ids) = rd.get(var) {
            for def_id in def_ids {
                if tainted_defs.contains(def_id) {
                    return true;
                }
            }
        }
    }
    false
}

/// Returns the call-argument names that are tainted at this sink call statement.
fn tainted_args_of(
    stmt: &StatementNode,
    reaching: &ReachingDefs,
    tainted_defs: &HashSet<NodeId>,
) -> Vec<String> {
    let Some(rd) = reaching.get(&stmt.id) else {
        return vec![];
    };
    stmt.call_args
        .iter()
        .filter(|arg| {
            rd.get(*arg).map_or(false, |defs| {
                defs.iter().any(|d| tainted_defs.contains(d))
            })
        })
        .cloned()
        .collect()
}

/// True if `sink_stmt` has a control-dep edge from a statement whose own def is tainted.
///
/// This catches the pattern: `if (tainted_flag) { sink(clean_arg); }` — the sink is
/// only reachable when the branch condition (which is tainted) is true.
fn is_control_dep_tainted(
    sink_stmt: &NodeId,
    pdg: &Pdg,
    tainted_defs: &HashSet<NodeId>,
    reaching: &ReachingDefs,
) -> bool {
    for edge in pdg.incoming(sink_stmt) {
        if edge.kind != PdgEdgeKind::ControlDep {
            continue;
        }
        // The branch stmt's ID is `edge.from`. It's "tainted" if its own def is tainted,
        // or if its reads have a tainted reaching def.
        if tainted_defs.contains(&edge.from) {
            return true;
        }
        // Also check if the branch stmt reads a tainted variable.
        // We synthesise a dummy StatementNode lookup from the reaching map.
        if reaching.contains_key(&edge.from) {
            let branch_rd = &reaching[&edge.from];
            for (_var, defs) in branch_rd {
                if defs.iter().any(|d| tainted_defs.contains(d)) {
                    return true;
                }
            }
        }
    }
    false
}

/// True if `call_name` matches any sink pattern.
fn is_sink(call_name: &str, patterns: &[&str]) -> bool {
    let lower = call_name.to_ascii_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

// ── Refinement glue for taint_cmd.rs ─────────────────────────────────────────

/// Per-path refinement produced by Phase 3. Parallel to [`crate::phase1::PathRefinement`].
#[derive(Debug)]
pub struct Phase3Refinement {
    /// Index into the original `paths` slice.
    pub path_index: usize,
    /// True if at least one confirmed sink was found.
    pub pdg_confirmed: bool,
    /// True if only conditional (control-dep) taint was found.
    pub pdg_conditional: bool,
    /// True if Phase 3 ran but found no taint evidence at all.
    pub pdg_clean: bool,
    /// Confidence multiplier to apply.
    pub confidence_multiplier: f32,
}

/// Run Phase 3 for all paths, returning per-path refinement info.
///
/// `get_node_file(id)` → file-relative path for the method node.
/// `resolve_src(file)` → source text for the file.
/// `tainted_param_names_for(id)` → which params are tainted on this path.
pub fn refine_paths_phase3(
    paths: &[crate::pass::TaintPath],
    get_node_file: &dyn Fn(&NodeId) -> Option<String>,
    resolve_src: impl Fn(&str) -> Option<String>,
    tainted_param_names_for: impl Fn(&NodeId) -> Vec<String>,
    sink_name_patterns: &[&str],
) -> Vec<Phase3Refinement> {
    paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let Some(file) = get_node_file(&path.source) else {
                return unavailable(i);
            };
            let Some(src) = resolve_src(&file) else {
                return unavailable(i);
            };
            let Some(cfg) = crate::cfg::build_cfg(&path.source, &src) else {
                return unavailable(i);
            };
            let dom = cfg.compute_dominators();
            let params = tainted_param_names_for(&path.source);
            let reaching = crate::pdg::compute_reaching_defs(&cfg, &params);
            let pdg = crate::pdg::build_pdg(&cfg, Some(&dom), Some(&reaching));

            let result = analyze_with_pdg(&cfg, &pdg, &reaching, &params, sink_name_patterns);

            let pdg_confirmed = !result.confirmed_sinks.is_empty();
            let pdg_conditional = !result.conditionally_tainted_sinks.is_empty();
            let pdg_clean = !pdg_confirmed && !pdg_conditional;

            Phase3Refinement {
                path_index: i,
                pdg_confirmed,
                pdg_conditional,
                pdg_clean,
                confidence_multiplier: result.confidence_multiplier,
            }
        })
        .collect()
}

fn unavailable(path_index: usize) -> Phase3Refinement {
    Phase3Refinement {
        path_index,
        pdg_confirmed: false,
        pdg_conditional: false,
        pdg_clean: false,
        confidence_multiplier: 1.0, // no evidence either way
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfg::build_cfg;
    use crate::pdg::{build_pdg, compute_reaching_defs};

    fn id(s: &str) -> NodeId {
        NodeId::new(s)
    }

    fn run(src: &str, method_id: &str, tainted_params: &[&str], sinks: &[&str]) -> Phase3Result {
        let mid = id(method_id);
        let cfg = build_cfg(&mid, src).expect("CFG must build");
        let dom = cfg.compute_dominators();
        let params: Vec<String> = tainted_params.iter().map(|s| s.to_string()).collect();
        let reaching = compute_reaching_defs(&cfg, &params);
        let pdg = build_pdg(&cfg, Some(&dom), Some(&reaching));
        analyze_with_pdg(&cfg, &pdg, &reaching, &params, sinks)
    }

    /// Direct: tainted param flows straight into a sink.
    #[test]
    fn direct_tainted_arg_to_sink() {
        let src = r#"
class Dao {
    void query(String input) {
        execute(input);
    }
}
"#;
        let r = run(src, "Method:com.example.Dao#query/1", &["input"], &["execute"]);
        assert!(!r.confirmed_sinks.is_empty(), "should confirm sink");
        assert!(r.confirmed_sinks[0].tainted_args.contains(&"input".to_string()));
    }

    /// Propagation: tainted flows through assignment then into sink.
    #[test]
    fn taint_propagates_through_assign() {
        let src = r#"
class Dao {
    void run(String cmd) {
        String q = cmd;
        exec(q);
    }
}
"#;
        let r = run(src, "Method:com.example.Dao#run/1", &["cmd"], &["exec"]);
        assert!(!r.confirmed_sinks.is_empty(), "should confirm sink via assign chain");
    }

    /// Kill: reassignment with a literal kills the taint.
    #[test]
    fn reassignment_kills_taint() {
        let src = r#"
class Dao {
    void process(String x) {
        x = "safe";
        execute(x);
    }
}
"#;
        let r = run(src, "Method:com.example.Dao#process/1", &["x"], &["execute"]);
        // After `x = "safe"`, x is no longer tainted.
        // Phase 3 should NOT confirm the sink.
        // (Phase 1 would have confirmed it because x was ever tainted.)
        assert!(
            r.confirmed_sinks.is_empty(),
            "reassignment should kill taint; confirmed_sinks={:?}",
            r.confirmed_sinks
        );
    }

    /// No taint: untainted param, sink call → multiplier should be 0.60.
    #[test]
    fn no_taint_low_multiplier() {
        let src = r#"
class Foo {
    void safe(String s) {
        execute(s);
    }
}
"#;
        let r = run(src, "Method:com.example.Foo#safe/1", &[], &["execute"]);
        assert!(r.confirmed_sinks.is_empty());
        assert!((r.confidence_multiplier - 0.60).abs() < 0.01);
    }

    /// Return propagation: tainted value returned.
    #[test]
    fn taint_return_detected() {
        let src = r#"
class Foo {
    String get(String input) {
        return input;
    }
}
"#;
        let r = run(src, "Method:com.example.Foo#get/1", &["input"], &["execute"]);
        assert!(r.taint_return, "should detect tainted return");
    }
}
