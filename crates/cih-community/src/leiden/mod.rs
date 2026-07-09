//! Leiden community detection: the crate-facing driver (this file) over a
//! vendored algorithm implementation (submodules).
//!
//! The vendored modules keep the full algorithm API (quality functions,
//! partition accessors, seeded runs) even where CIH only drives a subset,
//! and keep literature naming (CPM, RBER) — hence the scoped allows.

#[allow(dead_code)]
pub(crate) mod algorithm;
#[allow(dead_code)]
pub(crate) mod builder;
#[allow(dead_code)]
pub(crate) mod error;
#[allow(dead_code)]
pub(crate) mod graph_data;
#[allow(dead_code)]
pub(crate) mod move_components;
#[allow(dead_code)]
pub(crate) mod parallel;
#[allow(dead_code)]
pub(crate) mod partition;
#[allow(dead_code, clippy::upper_case_acronyms)]
pub(crate) mod quality;
#[allow(dead_code, clippy::upper_case_acronyms)]
pub(crate) mod runner;

use petgraph::graph::UnGraph;
use petgraph::visit::EdgeRef;

use crate::constants::MIN_EDGE_WEIGHT;
use builder::GraphDataBuilder;
use runner::{Leiden, LeidenConfig, QualityType};

/// Leiden over an undirected `f32`-weighted graph. Generic over the node weight `N` (unused here —
/// only graph structure + edge weights matter), so callers can key nodes by `NodeId` or a compact
/// `u32` index. Returns the community id per petgraph node index (0..node_count).
pub(crate) fn leiden<N>(
    graph: &UnGraph<N, f32>,
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
        let w = (*edge.weight() as f64).max(MIN_EDGE_WEIGHT as f64);
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
mod tests;
