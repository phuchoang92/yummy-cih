use cih_core::NodeId;
use petgraph::graph::UnGraph;
use petgraph::visit::EdgeRef;

use crate::leiden_impl::{GraphDataBuilder, Leiden, LeidenConfig, QualityType};

pub(crate) fn leiden(
    graph: &UnGraph<NodeId, f32>,
    resolution: f64,
    max_iterations: usize,
    seed: u64,
) -> Vec<usize> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }

    let mut builder = GraphDataBuilder::new(n);
    for edge in graph.edge_references() {
        let u = edge.source().index();
        let v = edge.target().index();
        let w = (*edge.weight() as f64).max(0.01);
        if u != v {
            let _ = builder.add_edge(u, v, w);
        }
    }
    let graph_data = match builder.build() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "leiden graph build failed; returning singleton partition");
            return (0..n).collect();
        }
    };

    let config = LeidenConfig::builder()
        .resolution(resolution)
        .max_iterations(max_iterations)
        .seed(seed)
        .quality(QualityType::Modularity)
        .build();

    match Leiden::new(config).run(&graph_data) {
        Ok(output) => (0..n).map(|i| output.partition.community_of(i)).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "leiden algorithm failed; returning singleton partition");
            (0..n).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::NodeId;
    use petgraph::graph::UnGraph;

    fn make_two_cliques() -> UnGraph<NodeId, f32> {
        let mut g: UnGraph<NodeId, f32> = UnGraph::new_undirected();
        let nodes: Vec<_> = (0..10)
            .map(|i| g.add_node(NodeId::new(format!("node{i}"))))
            .collect();
        // Clique A: nodes 0-4
        for i in 0..5 {
            for j in (i + 1)..5 {
                g.add_edge(nodes[i], nodes[j], 1.0);
            }
        }
        // Clique B: nodes 5-9
        for i in 5..10 {
            for j in (i + 1)..10 {
                g.add_edge(nodes[i], nodes[j], 1.0);
            }
        }
        // Weak bridge
        g.add_edge(nodes[0], nodes[5], 0.1);
        g
    }

    #[test]
    fn two_cliques_yield_two_communities() {
        let g = make_two_cliques();
        let assignments = leiden(&g, 1.0, 100, 42);
        assert_eq!(assignments.len(), 10);
        let comm_a = assignments[0];
        for i in 1..5 {
            assert_eq!(assignments[i], comm_a, "node {i} should be in clique A's community");
        }
        let comm_b = assignments[5];
        for i in 6..10 {
            assert_eq!(assignments[i], comm_b, "node {i} should be in clique B's community");
        }
        assert_ne!(comm_a, comm_b, "the two cliques should be in different communities");
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
}
