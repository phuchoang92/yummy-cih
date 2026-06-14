use std::collections::HashSet;

use cih_core::NodeId;
use petgraph::graph::{NodeIndex, UnGraph};

pub fn cohesion_score(
    members: &[NodeIndex],
    graph: &UnGraph<NodeId, f32>,
    sample_size: usize,
) -> f64 {
    if members.is_empty() {
        return 0.0;
    }
    let member_set: HashSet<NodeIndex> = members.iter().copied().collect();
    let mut total = 0usize;
    let mut internal = 0usize;
    for node in members.iter().take(sample_size).copied() {
        for neighbor in graph.neighbors(node) {
            total += 1;
            if member_set.contains(&neighbor) {
                internal += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        ((internal as f64) / (total as f64)).clamp(0.0, 1.0)
    }
}
