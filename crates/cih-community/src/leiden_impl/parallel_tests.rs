use super::*;
use crate::leiden_impl::builder::GraphDataBuilder;
use crate::leiden_impl::graph_data::GraphData;

fn make_two_cliques() -> GraphData {
    let mut b = GraphDataBuilder::new(10);
    for i in 0..5 {
        for j in (i + 1)..5 {
            b.add_edge(i, j, 1.0).unwrap();
        }
    }
    for i in 5..10 {
        for j in (i + 1)..10 {
            b.add_edge(i, j, 1.0).unwrap();
        }
    }
    b.add_edge(0, 5, 1.0).unwrap();
    b.build().unwrap()
}

#[test]
fn test_coloring_basic() {
    let data = make_two_cliques();
    let order: Vec<usize> = (0..data.node_count()).collect();
    let (colors, num_colors) = greedy_coloring(&data, &order);

    for node in 0..data.node_count() {
        let (targets, _) = data.neighbor_slices(node);
        for &neighbor in targets {
            if neighbor != node {
                assert_ne!(
                    colors[node], colors[neighbor],
                    "Adjacent nodes {} and {} have same color {}",
                    node, neighbor, colors[node]
                );
            }
        }
    }
    assert!(num_colors > 0);
}
