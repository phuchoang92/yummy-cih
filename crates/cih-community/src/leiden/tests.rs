use super::*;
use cih_core::NodeId;
use petgraph::graph::UnGraph;

fn make_two_cliques() -> UnGraph<NodeId, f32> {
    let mut g: UnGraph<NodeId, f32> = UnGraph::new_undirected();
    let nodes: Vec<_> = (0..10)
        .map(|i| g.add_node(NodeId::new(format!("node{i}"))))
        .collect();
    for i in 0..5 {
        for j in (i + 1)..5 {
            g.add_edge(nodes[i], nodes[j], 1.0);
        }
    }
    for i in 5..10 {
        for j in (i + 1)..10 {
            g.add_edge(nodes[i], nodes[j], 1.0);
        }
    }
    g.add_edge(nodes[0], nodes[5], 0.1);
    g
}

#[test]
fn two_cliques_yield_two_communities() {
    let g = make_two_cliques();
    let assignments = leiden(&g, 1.0, 100, 42);
    assert_eq!(assignments.len(), 10);
    let comm_a = assignments[0];
    for (i, &assignment) in assignments.iter().enumerate().take(5).skip(1) {
        assert_eq!(
            assignment, comm_a,
            "node {i} should be in clique A's community"
        );
    }
    let comm_b = assignments[5];
    for (i, &assignment) in assignments.iter().enumerate().take(10).skip(6) {
        assert_eq!(
            assignment, comm_b,
            "node {i} should be in clique B's community"
        );
    }
    assert_ne!(
        comm_a, comm_b,
        "the two cliques should be in different communities"
    );
}

#[test]
fn empty_graph_returns_empty() {
    let g: UnGraph<NodeId, f32> = UnGraph::new_undirected();
    let result = leiden(&g, 1.0, 100, 0);
    assert!(result.is_empty());
}

#[test]
fn single_node_returns_one_community() {
    let mut g: UnGraph<NodeId, f32> = UnGraph::new_undirected();
    g.add_node(NodeId::new("a"));
    let result = leiden(&g, 1.0, 100, 0);
    assert_eq!(result.len(), 1);
}
