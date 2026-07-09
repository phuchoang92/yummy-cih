//! Core graph data representation for the Leiden algorithm.
//!
//! [`GraphData`] stores a graph in CSR (Compressed Sparse Row) format with
//! separate out-edge and in-edge storage. For undirected graphs, only the
//! out-edge CSR is populated and the in-edge CSR is empty. For directed graphs,
//! both CSRs are populated.

/// Internal CSR graph representation for the Leiden algorithm.
///
/// Each undirected edge is stored twice in the out-edge CSR (once per direction)
/// so that iterating over all neighbors of a node is O(degree). For directed
/// graphs, out-edges and in-edges are stored in separate CSRs.
///
/// Construction is handled by [`crate::graph::GraphDataBuilder`].
#[derive(Debug, Clone)]
pub struct GraphData {
    pub(crate) n: usize,
    pub(crate) out_offsets: Vec<usize>,
    pub(crate) out_targets: Vec<usize>,
    pub(crate) out_weights: Vec<f64>,
    pub(crate) total_weight: f64,
    pub(crate) total_node_weight: f64,
    pub(crate) out_degree: Vec<f64>,
    pub(crate) node_weight: Vec<f64>,
    pub(crate) directed: bool,
    pub(crate) in_offsets: Vec<usize>,
    pub(crate) in_targets: Vec<usize>,
    pub(crate) in_weights: Vec<f64>,
    pub(crate) in_degree: Vec<f64>,
}

impl GraphData {
    /// Number of nodes in the graph.
    #[inline]
    pub fn node_count(&self) -> usize {
        self.n
    }

    /// Sum of all edge weights (each undirected edge counted once).
    #[inline]
    pub fn total_weight(&self) -> f64 {
        self.total_weight
    }

    /// Total weight of all nodes (sum of `node_weight`).
    #[inline]
    pub fn total_node_weight(&self) -> f64 {
        self.total_node_weight
    }

    /// Whether the graph is directed.
    #[inline]
    pub fn is_directed(&self) -> bool {
        self.directed
    }

    /// Iterate over all `(neighbor, weight)` pairs for a node.
    ///
    /// For undirected graphs, this returns all neighbors (out-edge CSR).
    /// For directed graphs, this returns out-edge neighbors.
    #[inline]
    pub fn neighbors(&self, node: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        let (targets, weights) = self.neighbor_slices(node);
        targets.iter().zip(weights.iter()).map(|(&t, &w)| (t, w))
    }

    /// Get raw slices of neighbor targets and weights for a node.
    ///
    /// For undirected graphs, returns out-edge slices.
    /// For directed graphs, returns out-edge slices.
    #[inline]
    pub fn neighbor_slices(&self, node: usize) -> (&[usize], &[f64]) {
        if node >= self.n {
            return (&[], &[]);
        }
        let start = self.out_offsets[node];
        let end = self.out_offsets[node + 1];
        (&self.out_targets[start..end], &self.out_weights[start..end])
    }

    /// Get the weighted degree of a node.
    ///
    /// For undirected graphs, returns the out-degree (which equals total degree).
    /// For directed graphs, returns `out_degree + in_degree`.
    #[inline]
    pub fn degree_of(&self, node: usize) -> f64 {
        if node >= self.n {
            return 0.0;
        }
        if self.directed {
            self.out_degree[node] + self.in_degree[node]
        } else {
            self.out_degree[node]
        }
    }

    /// Get the weight of a node (1.0 for original nodes, aggregated for super-nodes).
    #[inline]
    pub fn node_weight(&self, node: usize) -> f64 {
        if node >= self.n {
            return 0.0;
        }
        self.node_weight[node]
    }

    // ── Out-edge accessors ──

    /// Iterate over all out-edges `(target, weight)` for a node.
    #[inline]
    pub fn out_neighbors(&self, node: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        let (targets, weights) = self.out_neighbor_slices(node);
        targets.iter().zip(weights.iter()).map(|(&t, &w)| (t, w))
    }

    /// Get raw slices of out-edge targets and weights for a node.
    #[inline]
    pub fn out_neighbor_slices(&self, node: usize) -> (&[usize], &[f64]) {
        if node >= self.n {
            return (&[], &[]);
        }
        let start = self.out_offsets[node];
        let end = self.out_offsets[node + 1];
        (&self.out_targets[start..end], &self.out_weights[start..end])
    }

    /// Get the weighted out-degree of a node.
    #[inline]
    pub fn out_degree_of(&self, node: usize) -> f64 {
        if node >= self.n {
            return 0.0;
        }
        self.out_degree[node]
    }

    // ── In-edge accessors ──

    /// Iterate over all in-edges `(source, weight)` for a node.
    ///
    /// Returns an empty iterator for undirected graphs.
    #[inline]
    pub fn in_neighbors(&self, node: usize) -> impl Iterator<Item = (usize, f64)> + '_ {
        let (targets, weights) = self.in_neighbor_slices(node);
        targets.iter().zip(weights.iter()).map(|(&t, &w)| (t, w))
    }

    /// Get raw slices of in-edge targets and weights for a node.
    ///
    /// Returns empty slices for undirected graphs.
    #[inline]
    pub fn in_neighbor_slices(&self, node: usize) -> (&[usize], &[f64]) {
        if self.directed && node < self.in_offsets.len() - 1 {
            let start = self.in_offsets[node];
            let end = self.in_offsets[node + 1];
            (&self.in_targets[start..end], &self.in_weights[start..end])
        } else {
            (&[], &[])
        }
    }

    /// Get the weighted in-degree of a node.
    ///
    /// Returns `0.0` for undirected graphs.
    #[inline]
    pub fn in_degree_of(&self, node: usize) -> f64 {
        if self.directed && node < self.in_degree.len() {
            self.in_degree[node]
        } else {
            0.0
        }
    }
}

#[cfg(test)]
#[path = "graph_data_tests.rs"]
mod tests;
