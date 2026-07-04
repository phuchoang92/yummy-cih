//! Builder for constructing [`GraphData`] from edges and node weights.
//!
//! [`GraphDataBuilder`] is the single entry-point for creating a [`GraphData`]
//! instance. It accumulates edges and optional node weights, then builds the
//! internal CSR structure on [`build`](GraphDataBuilder::build).
//!
//! # Example
//!
//! ```ignore
//! use cih_community::leiden_impl::builder::GraphDataBuilder;
//!
//! let mut b = GraphDataBuilder::new(4);
//! b.add_edge(0, 1, 1.0).unwrap();
//! b.add_edge(1, 2, 2.0).unwrap();
//! b.add_edge(2, 3, 1.5).unwrap();
//! let graph = b.build().unwrap();
//!
//! assert_eq!(graph.node_count(), 4);
//! ```

use crate::leiden_impl::error::{LeidenError, Result};
use crate::leiden_impl::graph_data::GraphData;

/// Builder that accumulates edges and node weights, then produces a [`GraphData`].
///
/// This is the **only** way to construct a [`GraphData`] from raw edges. The
/// builder validates inputs incrementally (on each [`add_edge`] call) and then
/// builds the CSR layout in [`build`].
///
/// [`add_edge`]: GraphDataBuilder::add_edge
/// [`build`]: GraphDataBuilder::build
pub struct GraphDataBuilder {
    node_count: usize,
    directed: bool,
    edges: Vec<(usize, usize, f64)>,
    node_weights: Vec<f64>,
}

impl GraphDataBuilder {
    /// Create a new builder for a graph with `node_count` nodes.
    ///
    /// All node weights default to `1.0`, `directed` defaults to `false`,
    /// and the edge list starts empty.
    #[must_use = "constructor returns a new instance"]
    pub fn new(node_count: usize) -> Self {
        Self {
            node_count,
            directed: false,
            edges: Vec::new(),
            node_weights: vec![1.0; node_count],
        }
    }

    /// Set the graph to directed mode.
    ///
    pub fn directed(mut self) -> Self {
        self.directed = true;
        self
    }

    /// Add a weighted edge `(src, dst, weight)`.
    ///
    /// Returns `Err(LeidenError::InconsistentStructure)` if `src` or `dst` is
    /// out of range, or `Err(LeidenError::InvalidEdgeWeight)` if the weight is
    /// not finite and non-negative.
    pub fn add_edge(&mut self, src: usize, dst: usize, weight: f64) -> Result<&mut Self> {
        if !(weight.is_finite() && weight >= 0.0) {
            return Err(LeidenError::InvalidEdgeWeight { weight });
        }
        if src >= self.node_count || dst >= self.node_count {
            return Err(LeidenError::InconsistentStructure {
                message: format!(
                    "node ID {} exceeds node_count {}",
                    src.max(dst),
                    self.node_count
                ),
            });
        }
        self.edges.push((src, dst, weight));
        Ok(self)
    }

    /// Override the weight for a single node.
    ///
    /// Returns `Err(LeidenError::InconsistentStructure)` if `node` is out of
    /// range.
    pub fn set_node_weight(&mut self, node: usize, weight: f64) -> Result<&mut Self> {
        if node >= self.node_count {
            return Err(LeidenError::InconsistentStructure {
                message: format!("node ID {} exceeds node_count {}", node, self.node_count),
            });
        }
        self.node_weights[node] = weight;
        Ok(self)
    }

    /// Consume the builder and produce a [`GraphData`].
    ///
    /// Delegates to the appropriate CSR constructor based on the `directed`
    /// flag. In Phase 1 only undirected construction is supported.
    pub fn build(self) -> Result<GraphData> {
        if self.directed {
            build_directed_csr(self.node_count, self.edges, self.node_weights)
        } else {
            build_undirected_csr(self.node_count, self.edges, self.node_weights)
        }
    }
}

/// Build an undirected [`GraphData`] from an edge list.
///
/// Produces exactly the same CSR as the original `GraphData::from_edgelist`:
///
/// * Each edge `(u, v, w)` with `u != v` is stored twice — once in the
///   adjacency of `u` and once in `v`. Self-loops `(u, u, w)` are stored
///   once but contribute `2·w` to the degree.
/// * `total_weight = degree.sum() / 2`
/// * `in_*` fields are empty, `directed` is `false`.
fn build_undirected_csr(
    n: usize,
    mut edges: Vec<(usize, usize, f64)>,
    node_weights: Vec<f64>,
) -> Result<GraphData> {
    edges.sort_by_key(|a| (a.0, a.1));
    // Merge consecutive duplicate edges by summing weights
    edges = {
        let mut merged: Vec<(usize, usize, f64)> = Vec::with_capacity(edges.len());
        for edge in edges {
            if let Some(last) = merged.last_mut() {
                if last.0 == edge.0 && last.1 == edge.1 {
                    last.2 += edge.2;
                    continue;
                }
            }
            merged.push(edge);
        }
        merged
    };
    let mut neighbor_count: Vec<usize> = vec![0; n];
    for &(u, v, _) in &edges {
        neighbor_count[u] += 1;
        if u != v {
            neighbor_count[v] += 1;
        }
    }

    let mut out_offsets: Vec<usize> = Vec::with_capacity(n + 1);
    out_offsets.push(0);
    let mut total = 0;
    for &count in &neighbor_count {
        total += count;
        out_offsets.push(total);
    }

    let mut out_targets: Vec<usize> = vec![0; total];
    let mut out_weights: Vec<f64> = vec![0.0; total];
    let mut cursor: Vec<usize> = out_offsets[..n].to_vec();

    for &(u, v, w) in &edges {
        out_targets[cursor[u]] = v;
        out_weights[cursor[u]] = w;
        cursor[u] += 1;
        if u != v {
            out_targets[cursor[v]] = u;
            out_weights[cursor[v]] = w;
            cursor[v] += 1;
        }
    }

    // Derive degree from CSR weights (single source of truth).
    // Self-loops are stored once in the CSR but contribute 2×w to degree.
    let degree: Vec<f64> = (0..n)
        .map(|node| {
            let start = out_offsets[node];
            let end = out_offsets[node + 1];
            let row_sum: f64 = out_weights[start..end].iter().sum();
            let self_loop_sum: f64 = out_targets[start..end]
                .iter()
                .zip(out_weights[start..end].iter())
                .filter(|&(&t, _)| t == node)
                .map(|(_, &w)| w)
                .sum();
            row_sum + self_loop_sum
        })
        .collect();

    validate_csr(
        n,
        &out_offsets,
        &out_targets,
        &out_weights,
        &node_weights,
    )?;

    let total_weight = degree.iter().sum::<f64>() / 2.0;
    let total_node_weight: f64 = node_weights.iter().sum();

    Ok(GraphData {
        n,
        out_offsets,
        out_targets,
        out_weights,
        total_weight,
        total_node_weight,
        out_degree: degree,
        node_weight: node_weights,
        directed: false,
        in_offsets: Vec::new(),
        in_targets: Vec::new(),
        in_weights: Vec::new(),
        in_degree: Vec::new(),
    })
}

/// Build a directed [`GraphData`] from an edge list.
///
/// Each edge `(u, v, w)` is stored once in the out-edge CSR of `u` and once
/// in the in-edge CSR of `v`. Self-loops `(u, u, w)` are stored once in each CSR
/// and contribute `w` to both out-degree and in-degree.
///
/// `total_weight = sum of all edge weights` (each edge counted once).
fn build_directed_csr(
    n: usize,
    mut edges: Vec<(usize, usize, f64)>,
    node_weights: Vec<f64>,
) -> Result<GraphData> {
    edges.sort_by_key(|a| (a.0, a.1));
    // Merge consecutive duplicate edges by summing weights
    edges = {
        let mut merged: Vec<(usize, usize, f64)> = Vec::with_capacity(edges.len());
        for edge in edges {
            if let Some(last) = merged.last_mut() {
                if last.0 == edge.0 && last.1 == edge.1 {
                    last.2 += edge.2;
                    continue;
                }
            }
            merged.push(edge);
        }
        merged
    };
    // ── Out-edge CSR ──
    let mut out_neighbor_count: Vec<usize> = vec![0; n];
    for &(u, _v, _) in &edges {
        out_neighbor_count[u] += 1;
    }

    let mut out_offsets: Vec<usize> = Vec::with_capacity(n + 1);
    out_offsets.push(0);
    let mut total = 0;
    for &count in &out_neighbor_count {
        total += count;
        out_offsets.push(total);
    }

    let mut out_targets: Vec<usize> = vec![0; total];
    let mut out_weights: Vec<f64> = vec![0.0; total];
    let mut out_cursor: Vec<usize> = out_offsets[..n].to_vec();

    for &(u, v, w) in &edges {
        let idx = out_cursor[u];
        out_targets[idx] = v;
        out_weights[idx] = w;
        out_cursor[u] += 1;
    }

    // ── In-edge CSR ──
    let mut in_neighbor_count: Vec<usize> = vec![0; n];
    for &(_u, v, _) in &edges {
        in_neighbor_count[v] += 1;
    }

    let mut in_offsets: Vec<usize> = Vec::with_capacity(n + 1);
    in_offsets.push(0);
    total = 0;
    for &count in &in_neighbor_count {
        total += count;
        in_offsets.push(total);
    }

    let mut in_targets: Vec<usize> = vec![0; total];
    let mut in_weights: Vec<f64> = vec![0.0; total];
    let mut in_cursor: Vec<usize> = in_offsets[..n].to_vec();

    for &(u, v, w) in &edges {
        let idx = in_cursor[v];
        in_targets[idx] = u;
        in_weights[idx] = w;
        in_cursor[v] += 1;
    }

    // Derive out_degree and in_degree from CSR weights (single source of truth).
    let out_degree: Vec<f64> = (0..n)
        .map(|node| {
            let start = out_offsets[node];
            let end = out_offsets[node + 1];
            out_weights[start..end].iter().sum()
        })
        .collect();
    let in_degree: Vec<f64> = (0..n)
        .map(|node| {
            let start = in_offsets[node];
            let end = in_offsets[node + 1];
            in_weights[start..end].iter().sum()
        })
        .collect();

    validate_csr(
        n,
        &out_offsets,
        &out_targets,
        &out_weights,
        &node_weights,
    )?;
    validate_csr(
        n,
        &in_offsets,
        &in_targets,
        &in_weights,
        &node_weights,
    )?;

    let total_weight: f64 = edges.iter().map(|&(_, _, w)| w).sum();
    let total_node_weight: f64 = node_weights.iter().sum();

    Ok(GraphData {
        n,
        out_offsets,
        out_targets,
        out_weights,
        total_weight,
        total_node_weight,
        out_degree,
        node_weight: node_weights,
        directed: true,
        in_offsets,
        in_targets,
        in_weights,
        in_degree,
    })
}

/// Validate the structural invariants of a CSR representation.
///
/// Checks:
///
/// * `offsets.len() == n + 1`
/// * `targets.len() == weights.len()`
/// * `node_weight.len() == n`
/// * `offsets[0] == 0`
/// * `offsets[n] == targets.len()`
///
/// All failures produce [`LeidenError::InconsistentStructure`].
fn validate_csr(
    n: usize,
    offsets: &[usize],
    targets: &[usize],
    weights: &[f64],
    node_weight: &[f64],
) -> Result<()> {
    if offsets.len() != n + 1 {
        return Err(LeidenError::InconsistentStructure {
            message: format!("offsets length {} != n + 1 ({})", offsets.len(), n + 1),
        });
    }
    if targets.len() != weights.len() {
        return Err(LeidenError::InconsistentStructure {
            message: format!(
                "targets length {} != weights length {}",
                targets.len(),
                weights.len()
            ),
        });
    }
    if node_weight.len() != n {
        return Err(LeidenError::InconsistentStructure {
            message: format!("node_weight length {} != n ({})", node_weight.len(), n),
        });
    }
    if offsets[0] != 0 {
        return Err(LeidenError::InconsistentStructure {
            message: format!("offsets[0] must be 0, got {}", offsets[0]),
        });
    }
    if offsets[n] != targets.len() {
        return Err(LeidenError::InconsistentStructure {
            message: format!(
                "offsets[n] ({}) != targets.len() ({})",
                offsets[n],
                targets.len()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "builder_tests.rs"]
mod tests;
