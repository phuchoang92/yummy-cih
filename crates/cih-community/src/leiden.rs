use std::collections::HashMap;
use std::time::{Duration, Instant};

use cih_core::NodeId;
use petgraph::graph::{NodeIndex, UnGraph};
use petgraph::visit::EdgeRef;

use crate::prng::Mulberry32;

const TIMEOUT: Duration = Duration::from_secs(60);
const EPSILON: f64 = 1.0e-9;

pub fn louvain(
    graph: &UnGraph<NodeId, f32>,
    resolution: f64,
    max_iterations: u32,
    rng: &mut Mulberry32,
) -> Vec<usize> {
    let started = Instant::now();
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    if graph.edge_count() == 0 {
        return (0..n).collect();
    }

    let mut node_to_comm: Vec<usize> = (0..n).collect();
    let node_weights = node_weights(graph);
    let mut comm_degrees = node_weights.clone();
    let total_weight: f64 = graph.edge_weights().map(|w| *w as f64).sum();
    if total_weight <= EPSILON {
        return (0..n).collect();
    }

    let limit = if max_iterations == 0 {
        usize::MAX
    } else {
        max_iterations as usize
    };
    let mut iter = 0usize;
    loop {
        if started.elapsed() > TIMEOUT {
            return vec![0; n];
        }
        if iter >= limit {
            break;
        }
        iter += 1;

        let mut moved = false;
        let mut order: Vec<NodeIndex> = graph.node_indices().collect();
        rng.shuffle(&mut order);

        for node in order {
            let i = node.index();
            let current = node_to_comm[i];
            let degree = node_weights[i];
            if degree <= EPSILON {
                continue;
            }

            comm_degrees[current] -= degree;

            let mut weights_by_comm: HashMap<usize, f64> = HashMap::new();
            for edge in graph.edges(node) {
                let neighbor_comm = node_to_comm[edge.target().index()];
                *weights_by_comm.entry(neighbor_comm).or_default() += *edge.weight() as f64;
            }

            let mut best_comm = current;
            let mut best_gain = 0.0f64;
            for (candidate, weight_to_comm) in weights_by_comm {
                let gain = modularity_gain(
                    weight_to_comm,
                    degree,
                    comm_degrees.get(candidate).copied().unwrap_or(0.0),
                    total_weight,
                    resolution,
                );
                if gain > best_gain + EPSILON
                    || ((gain - best_gain).abs() <= EPSILON && candidate < best_comm)
                {
                    best_gain = gain;
                    best_comm = candidate;
                }
            }

            node_to_comm[i] = best_comm;
            comm_degrees[best_comm] += degree;
            if best_comm != current {
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }

    renumber_by_size(node_to_comm)
}

fn modularity_gain(
    weight_to_comm: f64,
    node_degree: f64,
    comm_degree: f64,
    total_weight: f64,
    resolution: f64,
) -> f64 {
    (weight_to_comm / total_weight)
        - resolution * (node_degree * comm_degree) / (2.0 * total_weight * total_weight)
}

fn node_weights(graph: &UnGraph<NodeId, f32>) -> Vec<f64> {
    let mut weights = vec![0.0; graph.node_count()];
    for edge in graph.edge_references() {
        let w = *edge.weight() as f64;
        weights[edge.source().index()] += w;
        weights[edge.target().index()] += w;
    }
    weights
}

fn renumber_by_size(assignments: Vec<usize>) -> Vec<usize> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for comm in &assignments {
        *counts.entry(*comm).or_default() += 1;
    }
    let mut communities: Vec<(usize, usize)> = counts.into_iter().collect();
    communities.sort_by(|(a_comm, a_count), (b_comm, b_count)| {
        b_count.cmp(a_count).then_with(|| a_comm.cmp(b_comm))
    });
    let remap: HashMap<usize, usize> = communities
        .into_iter()
        .enumerate()
        .map(|(new_idx, (old_idx, _))| (old_idx, new_idx))
        .collect();
    assignments
        .into_iter()
        .map(|old| remap.get(&old).copied().unwrap_or(0))
        .collect()
}
