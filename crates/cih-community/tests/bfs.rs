use std::collections::HashMap;

use cih_community::bfs::{encode_trace, is_subtrace_of, trace_process_paths};
use cih_community::ProcessConfig;
use cih_core::NodeId;
use petgraph::graph::{DiGraph, NodeIndex};

fn id(name: &str) -> NodeId {
    NodeId::new(format!("Method:{name}#{}/0", name.to_ascii_lowercase()))
}

fn cfg() -> ProcessConfig {
    ProcessConfig {
        max_trace_depth: 10,
        max_branching: 4,
        max_processes: 100,
        min_steps: 2,
        min_trace_confidence: 0.5,
        max_states_per_entry: 50_000,
    }
}

fn graph_with_nodes(names: &[&str]) -> (DiGraph<NodeId, f32>, Vec<NodeIndex>) {
    let mut graph = DiGraph::<NodeId, f32>::new();
    let indexes = names.iter().map(|name| graph.add_node(id(name))).collect();
    (graph, indexes)
}

fn entry(graph: &DiGraph<NodeId, f32>, idx: NodeIndex) -> Vec<(NodeId, f64)> {
    vec![(graph[idx].clone(), 1.0)]
}

#[test]
fn parent_pointer_bfs_prevents_cycles() {
    let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C"]);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    graph.add_edge(nodes[1], nodes[2], 1.0);
    graph.add_edge(nodes[2], nodes[0], 1.0);

    let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());

    assert_eq!(traces.len(), 1);
    assert_eq!(
        encode_trace(&traces[0], &graph),
        "Method:A#a/0->Method:B#b/0->Method:C#c/0"
    );
}

#[test]
fn parent_pointer_bfs_respects_max_branching() {
    let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
    graph.add_edge(nodes[0], nodes[3], 1.0);
    graph.add_edge(nodes[0], nodes[2], 1.0);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    let mut cfg = cfg();
    cfg.max_branching = 2;

    let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg);
    let encoded: Vec<_> = traces
        .iter()
        .map(|trace| encode_trace(trace, &graph))
        .collect();

    assert_eq!(encoded.len(), 2);
    assert!(encoded.iter().any(|s| s.ends_with("Method:B#b/0")));
    assert!(encoded.iter().any(|s| s.ends_with("Method:C#c/0")));
    assert!(!encoded.iter().any(|s| s.ends_with("Method:D#d/0")));
}

#[test]
fn parent_pointer_bfs_respects_max_trace_depth() {
    let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    graph.add_edge(nodes[1], nodes[2], 1.0);
    graph.add_edge(nodes[2], nodes[3], 1.0);
    let mut cfg = cfg();
    cfg.max_trace_depth = 3;

    let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg);

    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0].len(), 3);
    assert_eq!(
        encode_trace(&traces[0], &graph),
        "Method:A#a/0->Method:B#b/0->Method:C#c/0"
    );
}

#[test]
fn parent_pointer_bfs_respects_max_states_per_entry() {
    let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    graph.add_edge(nodes[0], nodes[2], 1.0);
    graph.add_edge(nodes[0], nodes[3], 1.0);
    let mut cfg = cfg();
    cfg.max_states_per_entry = 3;

    let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg);
    let encoded: Vec<_> = traces
        .iter()
        .map(|trace| encode_trace(trace, &graph))
        .collect();

    assert_eq!(encoded.len(), 2);
    assert!(encoded.iter().any(|s| s.ends_with("Method:B#b/0")));
    assert!(encoded.iter().any(|s| s.ends_with("Method:C#c/0")));
    assert!(!encoded.iter().any(|s| s.ends_with("Method:D#d/0")));
}

#[test]
fn parallel_edges_to_same_target_are_deduped() {
    let (mut graph, nodes) = graph_with_nodes(&["A", "B"]);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    graph.add_edge(nodes[0], nodes[1], 0.8);

    let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());
    assert_eq!(
        traces.len(),
        1,
        "parallel edges must not produce duplicate traces"
    );
}

#[test]
fn dedup_does_not_false_positive_on_shared_node_id_prefix() {
    let (mut graph, nodes) = graph_with_nodes(&["Root", "Pay", "PayService"]);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    graph.add_edge(nodes[0], nodes[2], 1.0);

    let traces = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());
    let encoded: Vec<_> = traces.iter().map(|t| encode_trace(t, &graph)).collect();

    assert!(
        encoded.iter().any(|s| s.ends_with("Method:Pay#pay/0")),
        "trace ending at Pay must not be incorrectly suppressed"
    );
    assert!(
        encoded
            .iter()
            .any(|s| s.ends_with("Method:PayService#payservice/0")),
        "trace ending at PayService must not be incorrectly suppressed"
    );
}

#[test]
fn is_subtrace_of_checks_segments_not_substring() {
    assert!(is_subtrace_of("A->B", "X->A->B->C"));
    assert!(!is_subtrace_of("A->B", "XA->XB->C"));
    assert!(is_subtrace_of("B", "A->B->C"));
    assert!(!is_subtrace_of("A->B->C->D", "A->B->C"));
}

#[test]
fn parent_pointer_bfs_is_deterministic() {
    let (mut graph, nodes) = graph_with_nodes(&["A", "B", "C", "D"]);
    graph.add_edge(nodes[0], nodes[3], 1.0);
    graph.add_edge(nodes[0], nodes[1], 1.0);
    graph.add_edge(nodes[0], nodes[2], 1.0);

    let first = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());
    let second = trace_process_paths(&graph, &entry(&graph, nodes[0]), &HashMap::new(), &cfg());

    assert_eq!(
        first
            .iter()
            .map(|trace| encode_trace(trace, &graph))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|trace| encode_trace(trace, &graph))
            .collect::<Vec<_>>()
    );
}
