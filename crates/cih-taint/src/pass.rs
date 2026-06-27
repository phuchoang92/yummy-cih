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

use std::collections::{HashMap, HashSet, VecDeque};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};

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
    let mut calls_fwd: HashMap<&NodeId, Vec<&NodeId>> = HashMap::new();
    for edge in edges {
        if edge.kind == EdgeKind::Calls {
            calls_fwd.entry(&edge.src).or_default().push(&edge.dst);
        }
    }

    // ── Source detection: methods with HandlesRoute or ListensTo outgoing edges ──
    let mut source_ids: HashSet<&NodeId> = HashSet::new();
    for edge in edges {
        if edge.kind == EdgeKind::HandlesRoute || edge.kind == EdgeKind::ListensTo {
            source_ids.insert(&edge.src);
        }
    }

    // ── Sink detection ────────────────────────────────────────────────────────

    // 1. Dynamic DbQuery nodes already flagged by emit_db_access.
    let dynamic_db_queries: HashSet<&NodeId> = nodes
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
    let mut sink_map: HashMap<&NodeId, SinkCategory> = HashMap::new();
    for edge in edges {
        match edge.kind {
            EdgeKind::ExecutesQuery if dynamic_db_queries.contains(&edge.dst) => {
                sink_map.entry(&edge.src).or_insert(SinkCategory::Sql);
            }
            EdgeKind::Calls => {
                let target = edge.dst.as_str();
                if let Some(rule) = rules.sinks.iter().find(|s| target.contains(s.node_id_pattern))
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
    let mut sanitizing_methods: HashSet<&NodeId> = HashSet::new();
    for edge in edges {
        if edge.kind != EdgeKind::Calls {
            continue;
        }
        let target = edge.dst.as_str();
        if rules
            .sanitizers
            .iter()
            .any(|s| target.contains(s.node_id_pattern))
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
        let mut visited: HashSet<&NodeId> = HashSet::new();

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
    // Direct source→sink: 0.9; each additional hop reduces by 0.05, floor 0.5.
    (0.9 - (edge_count.saturating_sub(1) as f32 * 0.05)).max(0.5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, Node, NodeKind, Range};

    fn method_node(id: &str) -> Node {
        Node {
            id: NodeId::new(id),
            kind: NodeKind::Method,
            name: id.split('#').last().unwrap_or(id).to_string(),
            qualified_name: Some(id.to_string()),
            file: "Test.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn db_query_node(id: &str, dynamic: bool) -> Node {
        Node {
            id: NodeId::new(id),
            kind: NodeKind::DbQuery,
            name: id.to_string(),
            qualified_name: None,
            file: "Test.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({ "dynamic": dynamic })),
        }
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge::new(NodeId::new(src), NodeId::new(dst), kind, 1.0, String::new())
    }

    #[test]
    fn direct_source_to_sql_sink_via_executes_query() {
        let nodes = vec![
            method_node("Method:com.example.OrderController#create/1"),
            method_node("Method:com.example.OrderDao#save/1"),
            db_query_node("DbQuery:OrderDao:10:5", true),
        ];
        let edges = vec![
            edge(
                "Method:com.example.OrderController#create/1",
                "Route:/api/orders",
                EdgeKind::HandlesRoute,
            ),
            edge(
                "Method:com.example.OrderController#create/1",
                "Method:com.example.OrderDao#save/1",
                EdgeKind::Calls,
            ),
            edge(
                "Method:com.example.OrderDao#save/1",
                "DbQuery:OrderDao:10:5",
                EdgeKind::ExecutesQuery,
            ),
        ];

        let rules = crate::rules::default_rules();
        let paths = find_taint_paths(&nodes, &edges, &rules);

        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0].source.as_str(),
            "Method:com.example.OrderController#create/1"
        );
        assert_eq!(
            paths[0].sink_method.as_str(),
            "Method:com.example.OrderDao#save/1"
        );
        assert_eq!(paths[0].category, SinkCategory::Sql);
        assert_eq!(paths[0].hops.len(), 2);
    }

    #[test]
    fn static_sql_not_a_sink() {
        let nodes = vec![
            method_node("Method:com.example.OrderController#create/1"),
            method_node("Method:com.example.OrderDao#save/1"),
            db_query_node("DbQuery:OrderDao:10:5", false), // static SQL
        ];
        let edges = vec![
            edge(
                "Method:com.example.OrderController#create/1",
                "Route:/api/orders",
                EdgeKind::HandlesRoute,
            ),
            edge(
                "Method:com.example.OrderController#create/1",
                "Method:com.example.OrderDao#save/1",
                EdgeKind::Calls,
            ),
            edge(
                "Method:com.example.OrderDao#save/1",
                "DbQuery:OrderDao:10:5",
                EdgeKind::ExecutesQuery,
            ),
        ];

        let rules = crate::rules::default_rules();
        let paths = find_taint_paths(&nodes, &edges, &rules);
        assert!(paths.is_empty(), "static SQL should not be a taint sink");
    }

    #[test]
    fn multi_hop_exec_sink() {
        let nodes = vec![
            method_node("Method:com.example.CommandController#run/1"),
            method_node("Method:com.example.CommandService#execute/1"),
            method_node("Method:java.lang.Runtime#exec/1"),
        ];
        let edges = vec![
            edge(
                "Method:com.example.CommandController#run/1",
                "Route:/api/run",
                EdgeKind::HandlesRoute,
            ),
            edge(
                "Method:com.example.CommandController#run/1",
                "Method:com.example.CommandService#execute/1",
                EdgeKind::Calls,
            ),
            edge(
                "Method:com.example.CommandService#execute/1",
                "Method:java.lang.Runtime#exec/1",
                EdgeKind::Calls,
            ),
        ];

        let rules = crate::rules::default_rules();
        let paths = find_taint_paths(&nodes, &edges, &rules);

        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].category, SinkCategory::Exec);
        // hops = [controller, service] (2 nodes, 1 edge_count → service IS the sink)
        assert_eq!(paths[0].sink_method.as_str(), "Method:com.example.CommandService#execute/1");
        assert_eq!(paths[0].edge_count(), 1);
    }

    #[test]
    fn sanitizer_stops_propagation() {
        let nodes = vec![
            method_node("Method:com.example.WebController#render/1"),
            method_node("Method:com.example.WebService#buildHtml/1"),
            method_node("Method:org.springframework.web.util.HtmlUtils#htmlEscape/1"),
        ];
        let edges = vec![
            edge(
                "Method:com.example.WebController#render/1",
                "Route:/render",
                EdgeKind::HandlesRoute,
            ),
            edge(
                "Method:com.example.WebController#render/1",
                "Method:com.example.WebService#buildHtml/1",
                EdgeKind::Calls,
            ),
            // WebService calls HtmlUtils.htmlEscape — marks it as sanitizing.
            edge(
                "Method:com.example.WebService#buildHtml/1",
                "Method:org.springframework.web.util.HtmlUtils#htmlEscape/1",
                EdgeKind::Calls,
            ),
        ];

        let rules = crate::rules::default_rules();
        let paths = find_taint_paths(&nodes, &edges, &rules);
        assert!(
            paths.is_empty(),
            "path through sanitizer should be suppressed"
        );
    }

    #[test]
    fn no_source_no_paths() {
        let nodes = vec![method_node("Method:com.example.Dao#save/1")];
        let edges = vec![edge(
            "Method:com.example.Dao#save/1",
            "DbQuery:Dao:5:1",
            EdgeKind::ExecutesQuery,
        )];
        let mut nodes_with_query = nodes;
        nodes_with_query.push(db_query_node("DbQuery:Dao:5:1", true));

        let rules = crate::rules::default_rules();
        let paths = find_taint_paths(&nodes_with_query, &edges, &rules);
        assert!(paths.is_empty(), "no source → no taint paths");
    }
}
