//! Phase 0 inter-procedural taint pass.
//!
//! BFS on `CALLS` edges from source methods (HTTP handlers / event listeners)
//! to sink methods (dynamic SQL, exec, file writes). No intra-procedural IR
//! required; operates entirely on the existing method-granularity call graph.
//!
//! Limitations (known, by design for Phase 0):
//! - No argument tracking: any path from source to sink is flagged regardless of
//!   whether the tainted value is actually passed through. Expect ~20-30% FP rate.
//! - Sanitizer detection stops an entire branch when a sanitizer CALL is seen on
//!   the path; it does not track which variables were sanitized.
//! - SQL parameterization (PreparedStatement) is not a named sanitizer method —
//!   this pass will miss the sanitization pattern and may flag false positives on
//!   repos that use parameterized queries consistently.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet, VecDeque};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};

use crate::confidence::{INTERPROC_BASE, INTERPROC_FLOOR, INTERPROC_HOP_PENALTY};
use crate::rules::{SinkCategory, TaintRules};

/// A single inter-procedural taint path found by the BFS.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TaintPath {
    /// Entry-point method where tainted data arrives (HTTP handler / event listener).
    pub source: NodeId,
    /// Method that performs the dangerous operation (SQL exec, OS exec, file write, …).
    pub sink_method: NodeId,
    /// Full method chain from source to sink_method (inclusive of both endpoints).
    pub hops: Vec<NodeId>,
    /// Category of the sink that was reached.
    pub category: SinkCategory,
    /// Confidence score (0.0–1.0). Shorter paths score higher.
    pub confidence: f32,
}

impl TaintPath {
    /// Number of call-graph edges traversed (hops minus 1 since hops includes source).
    pub fn edge_count(&self) -> usize {
        self.hops.len().saturating_sub(1)
    }

    /// Emit a `TaintFlow` graph edge for persistence into the main graph.
    pub fn to_edge(&self) -> Edge {
        Edge {
            src: self.source.clone(),
            dst: self.sink_method.clone(),
            kind: EdgeKind::TaintFlow,
            confidence: self.confidence,
            reason: format!("taint-phase0-{}", self.category.label()),
            props: Some(serde_json::json!({
                "hops": self.hops.iter().map(NodeId::as_str).collect::<Vec<_>>(),
                "hop_count": self.edge_count(),
                "sink_category": self.category.label(),
                "severity": self.category.severity(),
            })),
        }
    }
}

/// Run the Phase 0 inter-procedural taint pass over the call graph.
///
/// `nodes` and `edges` come directly from [`cih_core::GraphArtifacts::read_nodes`] /
/// [`read_edges`]. Returns one [`TaintPath`] per (source, sink) pair reachable within
/// `rules.max_hops` call-graph edges.
pub fn find_taint_paths(nodes: &[Node], edges: &[Edge], rules: &TaintRules) -> Vec<TaintPath> {
    // ── Index: CALLS forward adjacency ───────────────────────────────────────
    let mut calls_fwd: FxHashMap<&NodeId, Vec<&NodeId>> = FxHashMap::default();
    for edge in edges {
        if edge.kind == EdgeKind::Calls {
            calls_fwd.entry(&edge.src).or_default().push(&edge.dst);
        }
    }

    // ── Source detection: methods with HandlesRoute or ListensTo outgoing edges ──
    let mut source_ids: FxHashSet<&NodeId> = FxHashSet::default();
    for edge in edges {
        if edge.kind == EdgeKind::HandlesRoute || edge.kind == EdgeKind::ListensTo {
            source_ids.insert(&edge.src);
        }
    }

    // ── Sink detection ────────────────────────────────────────────────────────

    // 1. Dynamic DbQuery nodes already flagged by emit_db_access.
    let dynamic_db_queries: FxHashSet<&NodeId> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::DbQuery)
        .filter(|n| {
            n.props
                .as_ref()
                .and_then(|p| p.get("dynamic"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .map(|n| &n.id)
        .collect();

    // 2. Build sink_map: method node ID → sink category.
    //    A method is a sink if it ExecutesQuery on a dynamic DbQuery node, or if it
    //    Calls a node whose ID matches a known dangerous pattern.
    let mut sink_map: FxHashMap<&NodeId, SinkCategory> = FxHashMap::default();
    for edge in edges {
        match edge.kind {
            EdgeKind::ExecutesQuery if dynamic_db_queries.contains(&edge.dst) => {
                sink_map.entry(&edge.src).or_insert(SinkCategory::Sql);
            }
            EdgeKind::Calls => {
                let target = edge.dst.as_str();
                if let Some(rule) = rules.sinks.iter().find(|s| target.contains(s.node_id_pattern.as_str()))
                {
                    sink_map.entry(&edge.src).or_insert(rule.category);
                }
            }
            _ => {}
        }
    }

    // ── Sanitizer detection ───────────────────────────────────────────────────
    // A method is considered "sanitizing" if it calls a known sanitizer callee.
    // When BFS visits such a method, we stop expanding that branch.
    let mut sanitizing_methods: FxHashSet<&NodeId> = FxHashSet::default();
    for edge in edges {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        let target = edge.dst.as_str();
        if rules
            .sanitizers
            .iter()
            .any(|s| target.contains(s.node_id_pattern.as_str()))
        {
            sanitizing_methods.insert(&edge.src);
        }
    }

    tracing::debug!(
        sources = source_ids.len(),
        sinks = sink_map.len(),
        sanitizers = sanitizing_methods.len(),
        "taint pass indexes built"
    );

    // ── BFS from each source ──────────────────────────────────────────────────
    let mut paths: Vec<TaintPath> = Vec::new();

    for source in &source_ids {
        // path = full node chain from source (inclusive) to the current node.
        let mut queue: VecDeque<(&NodeId, Vec<&NodeId>)> = VecDeque::new();
        let mut visited: FxHashSet<&NodeId> = FxHashSet::default();

        queue.push_back((source, vec![source]));
        visited.insert(source);

        while let Some((current, path)) = queue.pop_front() {
            // Enforce hop limit (path includes source, so edge count = path.len()-1).
            if path.len() > rules.max_hops + 1 {
                continue;
            }

            // Check: current node is a sink.
            if let Some(&category) = sink_map.get(current) {
                let edge_count = path.len() - 1;
                paths.push(TaintPath {
                    source: (*source).clone(),
                    sink_method: (*current).clone(),
                    hops: path.iter().map(|n| (*n).clone()).collect(),
                    category,
                    confidence: confidence_for_edges(edge_count),
                });
                // Don't expand past the sink — we found what we were looking for.
                continue;
            }

            // Check: current method calls a sanitizer — stop this branch.
            if sanitizing_methods.contains(current) {
                continue;
            }

            // Expand to callees.
            if let Some(callees) = calls_fwd.get(current) {
                for callee in callees {
                    if !visited.contains(*callee) {
                        visited.insert(callee);
                        let mut new_path = path.clone();
                        new_path.push(callee);
                        queue.push_back((callee, new_path));
                    }
                }
            }
        }
    }

    tracing::info!(paths = paths.len(), "Phase 0 taint pass complete");
    paths
}

fn confidence_for_edges(edge_count: usize) -> f32 {
    (INTERPROC_BASE - (edge_count.saturating_sub(1) as f32 * INTERPROC_HOP_PENALTY)).max(INTERPROC_FLOOR)
}
