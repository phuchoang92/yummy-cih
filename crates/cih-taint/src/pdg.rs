//! Phase 3 (part 1): Program Dependence Graph (PDG) construction.
//!
//! A PDG has two kinds of edges:
//!
//! - **Data dependence** (`DataDep`): statement D defines variable `v`, statement U reads `v`,
//!   and D's definition reaches U with no intervening redefinition of `v`. Computed via
//!   *reaching-definition analysis* (a classic bit-vector data-flow problem).
//!
//! - **Control dependence** (`ControlDep`): statement S is control-dependent on branch B if S
//!   is in the "control region" of B — i.e., reachable from one of B's True/False successors
//!   but not necessarily from all successors. Computed from the dominance tree: all blocks
//!   dominated by a True/False/Back successor of B (but not by B's common post-join) are in
//!   B's control region. For Java's structured control flow this is equivalent to the classical
//!   post-dominator-based criterion.
//!
//! All data structures are purely in-memory; nothing is persisted to the main graph.

use std::collections::HashMap;

use cih_core::NodeId;

use crate::cfg::{Cfg, CfgEdgeKind, DomTree};
use crate::ir::StatementKind;

// ── Public types ──────────────────────────────────────────────────────────────

/// The kind of a PDG edge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PdgEdgeKind {
    /// True (read-after-write) data dependence: `from` defines `var`, `to` reads it.
    DataDep {
        var: String,
    },
    /// Control dependence: `to` executes only on some successors of `from` (a branch/loop).
    ControlDep,
}

/// A directed edge in the PDG.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PdgEdge {
    /// Statement node ID of the source (the definition or the branch condition).
    pub from: NodeId,
    /// Statement node ID of the target (the use or the control-dependent statement).
    pub to: NodeId,
    pub kind: PdgEdgeKind,
}

/// In-memory Program Dependence Graph for a single method.
pub struct Pdg {
    pub callable_id: NodeId,
    /// All edges (both data and control dependence).
    pub edges: Vec<PdgEdge>,
    /// Index: target statement → incoming edges (for taint propagation).
    by_target: HashMap<NodeId, Vec<usize>>,
    /// Index: source statement → outgoing edges (for impact analysis).
    by_source: HashMap<NodeId, Vec<usize>>,
}

impl Pdg {
    fn new(callable_id: NodeId, edges: Vec<PdgEdge>) -> Self {
        let mut by_target: HashMap<NodeId, Vec<usize>> = HashMap::new();
        let mut by_source: HashMap<NodeId, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            by_target.entry(e.to.clone()).or_default().push(i);
            by_source.entry(e.from.clone()).or_default().push(i);
        }
        Self { callable_id, edges, by_target, by_source }
    }

    /// Edges that terminate at `target` (what does this statement depend on?).
    pub fn incoming(&self, target: &NodeId) -> impl Iterator<Item = &PdgEdge> {
        self.by_target
            .get(target)
            .into_iter()
            .flat_map(|idxs| idxs.iter().map(|&i| &self.edges[i]))
    }

    /// Edges that originate at `source` (what does this statement affect?).
    pub fn outgoing(&self, source: &NodeId) -> impl Iterator<Item = &PdgEdge> {
        self.by_source
            .get(source)
            .into_iter()
            .flat_map(|idxs| idxs.iter().map(|&i| &self.edges[i]))
    }

    /// All data-dependence edges.
    pub fn data_edges(&self) -> impl Iterator<Item = &PdgEdge> {
        self.edges.iter().filter(|e| matches!(e.kind, PdgEdgeKind::DataDep { .. }))
    }

    /// All control-dependence edges.
    pub fn control_edges(&self) -> impl Iterator<Item = &PdgEdge> {
        self.edges.iter().filter(|e| e.kind == PdgEdgeKind::ControlDep)
    }
}

// ── Reaching definitions ──────────────────────────────────────────────────────

/// For each statement, the set of (variable → reaching definition stmt IDs) that
/// are live at the statement's entry point.
///
/// Virtual IDs for method parameters are encoded as `"{callable_id}:param:{name}"`.
pub type ReachingDefs = HashMap<NodeId, HashMap<String, Vec<NodeId>>>;

/// Build the virtual parameter-definition node ID for `param_name` in `callable_id`.
pub fn param_def_id(callable_id: &NodeId, param_name: &str) -> NodeId {
    NodeId::new(format!("{}:param:{param_name}", callable_id.as_str()))
}

/// Compute reaching definitions for every statement in `cfg`.
///
/// `param_names`: names of parameters that arrive as definitions at the method entry.
/// These are represented as virtual definition nodes (see [`param_def_id`]).
pub fn compute_reaching_defs(cfg: &Cfg, param_names: &[String]) -> ReachingDefs {
    let callable_id = &cfg.callable_id;

    // Virtual definition IDs for each parameter.
    let param_defs: HashMap<String, NodeId> = param_names
        .iter()
        .map(|p| (p.clone(), param_def_id(callable_id, p)))
        .collect();

    // OUT[block_id]: the reaching definitions that exit each block.
    // Maps var_name → sorted Vec of definition stmt IDs (a "gen-kill" set).
    let mut out: HashMap<crate::cfg::BlockId, HashMap<String, Vec<NodeId>>> = HashMap::new();

    let rpo = cfg.reverse_post_order();

    // Iterative data-flow: iterate until OUT sets stop growing.
    let mut changed = true;
    while changed {
        changed = false;

        for block_id in &rpo {
            let block = match cfg.block(block_id) {
                Some(b) => b,
                None => continue,
            };

            // IN[B] = param_defs (if entry) ∪ ⋃ OUT[P] for predecessors P.
            let mut current = merge_in(
                block_id == &cfg.entry,
                &param_defs,
                &block.preds,
                &out,
            );

            // Transfer function: for each statement, kill old defs of written vars, gen new def.
            for stmt in &block.stmts {
                for written_var in &stmt.writes {
                    current.insert(written_var.clone(), vec![stmt.id.clone()]);
                }
            }

            // Check if OUT[B] grew.
            let old_out = out.get(block_id);
            let new_count: usize = current.values().map(|v| v.len()).sum();
            let old_count: usize = old_out
                .map(|o| o.values().map(|v| v.len()).sum())
                .unwrap_or(0);

            if new_count > old_count || old_out.is_none() {
                out.insert(block_id.clone(), current);
                changed = true;
            } else {
                // Same count but might differ in which defs — do a full equality check.
                if old_out != Some(&current) {
                    out.insert(block_id.clone(), current);
                    changed = true;
                }
            }
        }
    }

    // Second pass: compute per-statement reaching defs (IN[stmt]).
    let mut result = ReachingDefs::new();

    for block_id in &rpo {
        let block = match cfg.block(block_id) {
            Some(b) => b,
            None => continue,
        };

        let mut current = merge_in(
            block_id == &cfg.entry,
            &param_defs,
            &block.preds,
            &out,
        );

        for stmt in &block.stmts {
            result.insert(stmt.id.clone(), current.clone());
            for written_var in &stmt.writes {
                current.insert(written_var.clone(), vec![stmt.id.clone()]);
            }
        }
    }

    result
}

/// Merge predecessor OUT sets and (optionally) initial param defs.
fn merge_in(
    is_entry: bool,
    param_defs: &HashMap<String, NodeId>,
    preds: &[crate::cfg::BlockId],
    out: &HashMap<crate::cfg::BlockId, HashMap<String, Vec<NodeId>>>,
) -> HashMap<String, Vec<NodeId>> {
    let mut merged: HashMap<String, Vec<NodeId>> = HashMap::new();

    if is_entry {
        for (var, def_id) in param_defs {
            merged.entry(var.clone()).or_default().push(def_id.clone());
        }
    }

    for pred_id in preds {
        if let Some(out_pred) = out.get(pred_id) {
            for (var, defs) in out_pred {
                let entry = merged.entry(var.clone()).or_default();
                for d in defs {
                    if !entry.contains(d) {
                        entry.push(d.clone());
                    }
                }
            }
        }
    }

    // Sort each def-list for deterministic comparison.
    for defs in merged.values_mut() {
        defs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    }

    merged
}

// ── Data dependence edges ─────────────────────────────────────────────────────

fn data_dep_edges(cfg: &Cfg, reaching: &ReachingDefs) -> Vec<PdgEdge> {
    let mut edges = Vec::new();

    for block in &cfg.blocks {
        for stmt in &block.stmts {
            let Some(rd) = reaching.get(&stmt.id) else { continue };

            // Collect all variables read by this statement (reads + call_args).
            let mut vars_read: Vec<&String> = stmt.reads.iter().collect();
            for arg in &stmt.call_args {
                if !vars_read.contains(&arg) {
                    vars_read.push(arg);
                }
            }

            for var in vars_read {
                if let Some(def_stmts) = rd.get(var) {
                    for def_id in def_stmts {
                        edges.push(PdgEdge {
                            from: def_id.clone(),
                            to: stmt.id.clone(),
                            kind: PdgEdgeKind::DataDep { var: var.clone() },
                        });
                    }
                }
            }
        }
    }

    dedup_edges(&mut edges);
    edges
}

// ── Control dependence edges ──────────────────────────────────────────────────

/// Compute control-dependence edges using the dominance tree.
///
/// Statement S is control-dependent on branch B if S is in the control region of B,
/// defined as: S is dominated by some True/False/Back successor of B, but not by B's
/// "join" block. For structured Java code this is equivalent to the classical
/// post-dominator-based definition.
///
/// Algorithm: for each branch/loop block B with a True/False edge to successor T,
/// add ControlDep edges from B's branch stmt to all stmts in blocks that are
/// strictly dominated by T.
fn control_dep_edges(cfg: &Cfg, dom: &DomTree) -> Vec<PdgEdge> {
    let mut edges = Vec::new();

    for block in &cfg.blocks {
        // Find the branch/loop statement in this block (if any).
        let Some(branch_stmt) = block.stmts.iter().find(|s| {
            matches!(s.kind, StatementKind::Branch | StatementKind::Loop)
        }) else {
            continue;
        };

        // For each True/False/Back successor: all blocks dominated by that successor
        // are in the control region of this branch.
        for (succ_id, edge_kind) in &block.succs {
            if matches!(edge_kind, CfgEdgeKind::Sequential | CfgEdgeKind::Exception) {
                continue;
            }
            // Add control-dep edges to all stmts in blocks dominated by succ_id.
            for other_block in &cfg.blocks {
                let in_region = other_block.id == *succ_id
                    || dom.strictly_dominates(succ_id, &other_block.id);
                if !in_region {
                    continue;
                }
                for stmt in &other_block.stmts {
                    if stmt.id == branch_stmt.id {
                        continue; // skip self-loops
                    }
                    edges.push(PdgEdge {
                        from: branch_stmt.id.clone(),
                        to: stmt.id.clone(),
                        kind: PdgEdgeKind::ControlDep,
                    });
                }
            }
        }
    }

    dedup_edges(&mut edges);
    edges
}

fn dedup_edges(edges: &mut Vec<PdgEdge>) {
    edges.sort_by(|a, b| {
        a.from
            .as_str()
            .cmp(b.from.as_str())
            .then(a.to.as_str().cmp(b.to.as_str()))
    });
    edges.dedup();
}

// ── Public constructor ────────────────────────────────────────────────────────

/// Build the complete PDG for `cfg` given a pre-computed dominance tree and
/// reaching-definition analysis.
///
/// Pass `None` for `reaching` to skip data-dependence edges (useful when only
/// control-dependence is needed). Pass `None` for `dom` to skip control-dependence.
pub fn build_pdg(
    cfg: &Cfg,
    dom: Option<&DomTree>,
    reaching: Option<&ReachingDefs>,
) -> Pdg {
    let mut all_edges = Vec::new();

    if let Some(rd) = reaching {
        all_edges.extend(data_dep_edges(cfg, rd));
    }
    if let Some(d) = dom {
        all_edges.extend(control_dep_edges(cfg, d));
    }

    dedup_edges(&mut all_edges);
    Pdg::new(cfg.callable_id.clone(), all_edges)
}

