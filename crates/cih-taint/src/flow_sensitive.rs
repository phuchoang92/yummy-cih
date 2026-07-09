//! Phase 3: PDG-based flow-sensitive, kill-aware taint analysis.
//!
//! Unlike Phase 1 (which tracks "is variable X tainted?"), Phase 3 tracks
//! "is *definition D* tainted?" This enables two kinds of kill:
//!
//! **Kill by reassignment** — a literal or computed-clean value replaces a tainted def:
//! ```java
//! String x = userInput;   // def-1 of x is tainted
//! x = "literal";          // def-2 of x is clean — KILLS the tainted def
//! sink(x);                // reaches def-2 only → NOT a confirmed sink
//! ```
//!
//! **Kill by sanitizer call** — a known sanitizer method produces a clean def even if
//! its arguments are tainted:
//! ```java
//! String safe = htmlEscape(userInput);  // sanitizer → clean def
//! print(safe);                          // reaches clean def only → NOT a confirmed sink
//! ```
//!
//! Phase 1 would flag both patterns as confirmed sinks. Phase 3 does not.
//!
//! # Algorithm
//!
//! 1. Initialize: mark the virtual param-def node IDs for each tainted parameter.
//! 2. Forward propagation (RPO order, reaching-defs already computed):
//!    - For each statement S: if S is a sanitizer call, its def is clean regardless of inputs.
//!    - Otherwise: if any reaching def of a variable that S reads is tainted,
//!      then S's own def is also tainted.
//!    - Propagate until fixpoint (needed for loops).
//! 3. Classify sinks:
//!    - **Confirmed** (`DataDep` chain): a tainted def reaches an argument of a sink call.
//!    - **Conditional** (`ControlDep` only): the sink is inside a control region whose
//!      branch condition is tainted, but no tainted def reaches the arg directly.
//!
//! The result feeds back to `taint_cmd.rs`, which applies a confidence multiplier to
//! the Phase 0 path score.

use rustc_hash::FxHashSet;
use std::collections::HashSet;

use cih_core::NodeId;

use crate::cfg::Cfg;
use crate::confidence::{PDG_CLEAN, PDG_CONDITIONAL, PDG_CONFIRMED};
use crate::ir::{StatementKind, StatementNode};
use crate::pdg::{param_def_id, Pdg, PdgEdgeKind, ReachingDefs};

// ── Public types ──────────────────────────────────────────────────────────────

/// A sink call confirmed reachable by a tainted data-def chain.
#[derive(Debug, Clone)]
pub struct PdgSink {
    /// Statement node ID of the sink call.
    pub stmt_id: NodeId,
    /// Callee name as extracted from the AST.
    pub call_name: String,
    /// Arguments to the sink that were tainted.
    pub tainted_args: Vec<String>,
}

/// Full Phase 3 result for a single method analysis.
#[derive(Debug)]
pub struct PdgResult {
    pub callable_id: NodeId,
    /// Sinks confirmed via a tainted data-dependence chain.
    pub confirmed_sinks: Vec<PdgSink>,
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
/// - `sanitizer_patterns`: node-id patterns from `TaintRules::sanitizers`
///   (e.g. `"HtmlUtils#htmlEscape"`). A call whose name appears as a substring of any
///   pattern is treated as a sanitizer: its write is a clean def.
pub fn analyze_with_pdg(
    cfg: &Cfg,
    pdg: &Pdg,
    reaching: &ReachingDefs,
    tainted_params: &[String],
    sink_name_patterns: &[&str],
    sanitizer_patterns: &[&str],
) -> PdgResult {
    let callable_id = cfg.callable_id.clone();

    // Seed: virtual parameter-definition node IDs are initially tainted.
    let mut tainted_defs: FxHashSet<NodeId> = tainted_params
        .iter()
        .map(|p| param_def_id(&callable_id, p))
        .collect();

    // Iterate to fixpoint (needed for loops — a tainted def in the loop body
    // might flow back through a back edge and re-reach earlier stmts).
    let rpo = cfg.reverse_post_order();
    let mut changed = true;
    while changed {
        changed = false;
        changed |= propagate_pass(cfg, reaching, &rpo, &mut tainted_defs, sanitizer_patterns);
    }

    // Classify statements.
    let mut confirmed_sinks = Vec::new();
    let mut conditionally_tainted_sinks = Vec::new();
    let mut taint_return = false;

    for block in &cfg.blocks {
        for stmt in &block.stmts {
            match stmt.kind {
                // Check both standalone calls and assignment-RHS calls (e.g. `String r = stmt.execute(sql)`).
                StatementKind::Call | StatementKind::Assign => {
                    let call_name = stmt.call_site.as_deref().unwrap_or("");
                    if !call_name.is_empty() && is_sink(call_name, sink_name_patterns) {
                        let tainted_args = tainted_args_of(stmt, reaching, &tainted_defs);
                        if !tainted_args.is_empty() {
                            confirmed_sinks.push(PdgSink {
                                stmt_id: stmt.id.clone(),
                                call_name: call_name.to_string(),
                                tainted_args,
                            });
                        } else if is_control_dep_tainted(
                            &stmt.id,
                            pdg,
                            cfg,
                            &tainted_defs,
                            reaching,
                        ) {
                            conditionally_tainted_sinks.push(stmt.id.clone());
                        }
                    }
                }
                StatementKind::Return if stmt_reads_tainted(stmt, reaching, &tainted_defs) => {
                    taint_return = true;
                }
                _ => {}
            }
        }
    }

    let confidence_multiplier = if !confirmed_sinks.is_empty() {
        PDG_CONFIRMED
    } else if !conditionally_tainted_sinks.is_empty() {
        PDG_CONDITIONAL
    } else {
        PDG_CLEAN
    };

    PdgResult {
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
    tainted_defs: &mut FxHashSet<NodeId>,
    sanitizer_patterns: &[&str],
) -> bool {
    let mut changed = false;
    for block_id in rpo {
        let Some(block) = cfg.block(block_id) else {
            continue;
        };
        for stmt in &block.stmts {
            if stmt.writes.is_empty() {
                continue;
            }
            // Sanitizer calls produce a clean def regardless of their inputs.
            // Skip adding to tainted_defs: the write gets a clean def, which kills
            // any prior tainted def of the same variable (via reaching-defs kill semantics).
            if is_sanitizer(stmt.call_site.as_deref().unwrap_or(""), sanitizer_patterns) {
                continue;
            }
            // If any variable read by this stmt has a tainted reaching def,
            // then all variables *written* by this stmt are now tainted (the def is tainted).
            if stmt_reads_tainted(stmt, reaching, tainted_defs)
                && tainted_defs.insert(stmt.id.clone())
            {
                changed = true;
            }
        }
    }
    changed
}

/// True if any variable in `stmt.reads` or `stmt.call_args` has a tainted reaching def.
fn stmt_reads_tainted(
    stmt: &StatementNode,
    reaching: &ReachingDefs,
    tainted_defs: &FxHashSet<NodeId>,
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
    tainted_defs: &FxHashSet<NodeId>,
) -> Vec<String> {
    let Some(rd) = reaching.get(&stmt.id) else {
        return vec![];
    };
    stmt.call_args
        .iter()
        .filter(|arg| {
            rd.get(*arg)
                .is_some_and(|defs| defs.iter().any(|d| tainted_defs.contains(d)))
        })
        .cloned()
        .collect()
}

/// True if `sink_stmt` has a control-dep edge from a branch whose condition reads a tainted def.
///
/// This catches the pattern: `if (tainted_flag) { sink(clean_arg); }` — the sink is
/// only reachable when the branch condition (which is tainted) is true.
///
/// `cfg` is needed to retrieve the branch statement's `reads` set so that we only check
/// variables the condition actually reads, rather than every variable live at that point
/// (which would cause false positives whenever any unrelated tainted variable is in scope).
fn is_control_dep_tainted(
    sink_stmt: &NodeId,
    pdg: &Pdg,
    cfg: &Cfg,
    tainted_defs: &FxHashSet<NodeId>,
    reaching: &ReachingDefs,
) -> bool {
    for edge in pdg.incoming(sink_stmt) {
        if edge.kind != PdgEdgeKind::ControlDep {
            continue;
        }
        // The branch stmt's ID is `edge.from`. It's tainted if its own def is tainted.
        if tainted_defs.contains(&edge.from) {
            return true;
        }
        // Check whether any variable that the branch condition actually *reads* has a
        // tainted reaching def. Restrict to branch.reads to avoid false positives from
        // unrelated tainted locals that happen to be live at the branch point.
        let Some(branch_rd) = reaching.get(&edge.from) else {
            continue;
        };
        // If the branch statement isn't in the CFG (e.g. synthetic node from a
        // different snapshot), skip rather than defaulting to an empty reads set.
        // An empty set would make `branch_reads.is_empty()` always true and flag
        // every tainted-def live at this point as a false-positive control dep.
        let Some(branch_stmt) = cfg.stmt_by_id(&edge.from) else {
            continue;
        };
        let branch_reads: HashSet<&String> = branch_stmt.reads.iter().collect();
        for (var, defs) in branch_rd {
            if branch_reads.contains(var)
                && defs.iter().any(|d| tainted_defs.contains(d))
            {
                return true;
            }
        }
    }
    false
}

/// True if `call_name` matches any sink pattern (pattern substring found in call name).
fn is_sink(call_name: &str, patterns: &[&str]) -> bool {
    let lower = call_name.to_ascii_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

/// True if `call_name` matches any sanitizer node-id pattern.
///
/// Sanitizer patterns look like `"HtmlUtils#htmlEscape"`. We extract the method-name
/// portion (after `#`) and require an exact case-insensitive match with `call_name`.
/// Exact matching prevents a short name like `"set"` from spuriously matching
/// `"PreparedStatement#setString"` and suppressing a real SQL-injection sink.
fn is_sanitizer(call_name: &str, node_id_patterns: &[&str]) -> bool {
    if call_name.is_empty() {
        return false;
    }
    let lower = call_name.to_ascii_lowercase();
    node_id_patterns.iter().any(|p| {
        let p_lower = p.to_ascii_lowercase();
        let method = p_lower.split('#').next_back().unwrap_or(&p_lower);
        lower == method
    })
}

// ── Refinement glue for taint_cmd.rs ─────────────────────────────────────────

/// Per-path refinement produced by Phase 3. Parallel to [`crate::liveness::PathRefinement`].
#[derive(Debug)]
pub struct PdgRefinement {
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
///
/// Tainted parameters are derived automatically from the CFG: since `path.source` is
/// an HTTP-handler / event-listener (identified by Phase 0), all of its formal parameters
/// arrive with untrusted data and are therefore seeded as tainted.
pub fn refine_paths(
    paths: &[crate::interproc::TaintPath],
    get_node_file: &dyn Fn(&NodeId) -> Option<String>,
    resolve_src: impl Fn(&str) -> Option<String>,
    sink_name_patterns: &[&str],
    sanitizer_patterns: &[&str],
) -> Vec<PdgRefinement> {
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
            // All params of the source (HTTP handler) are tainted — they carry user input.
            let reaching = crate::pdg::compute_reaching_defs(&cfg, &cfg.param_names);
            let pdg = crate::pdg::build_pdg(&cfg, Some(&dom), Some(&reaching));

            let result = analyze_with_pdg(
                &cfg,
                &pdg,
                &reaching,
                &cfg.param_names,
                sink_name_patterns,
                sanitizer_patterns,
            );

            let pdg_confirmed = !result.confirmed_sinks.is_empty();
            let pdg_conditional = !result.conditionally_tainted_sinks.is_empty();
            let pdg_clean = !pdg_confirmed && !pdg_conditional;

            PdgRefinement {
                path_index: i,
                pdg_confirmed,
                pdg_conditional,
                pdg_clean,
                confidence_multiplier: result.confidence_multiplier,
            }
        })
        .collect()
}

fn unavailable(path_index: usize) -> PdgRefinement {
    PdgRefinement {
        path_index,
        pdg_confirmed: false,
        pdg_conditional: false,
        pdg_clean: false,
        confidence_multiplier: 1.0, // no evidence either way
    }
}
