//! Quality functions for community detection.

pub use crate::leiden_impl::graph_data::GraphData;
pub use crate::leiden_impl::move_components::MoveComponents;

/// Trait for quality functions used by the Leiden algorithm.
pub trait QualityFunction {
    /// Compute the quality delta of moving a node, given precomputed components.
    fn delta_move_from_components(&self, c: &MoveComponents) -> f64;

    /// Compute the total quality of a partition.
    fn total_quality(&self, data: &GraphData, partition: &crate::leiden_impl::partition::Partition) -> f64;
}

/// Modularity: Q = Σ_c [e_c/m - γ*(Σ_c/(2m))²]
pub struct Modularity {
    /// Resolution parameter γ.
    pub resolution: f64,
}

impl Modularity {
    /// Create a new Modularity with default resolution (1.0).
    #[must_use = "constructor returns a new instance"]
    pub fn new() -> Self {
        Self { resolution: 1.0 }
    }

    /// Create a new Modularity with a custom resolution parameter.
    #[must_use = "constructor returns a new instance"]
    pub fn with_resolution(resolution: f64) -> Self {
        Self { resolution }
    }
}

impl Default for Modularity {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn modularity_delta(resolution: f64, c: &MoveComponents) -> f64 {
    if c.two_m == 0.0 {
        return 0.0;
    }
    if !c.directed {
        (c.k_v_to_target_out - c.k_v_to_current_out) * 2.0 / c.two_m
            - resolution
                * c.k_v_out
                * (c.sigma_tot_target_out - c.sigma_tot_current_out + c.k_v_out)
                * 2.0
                / (c.two_m * c.two_m)
    } else {
        let m = c.two_m / 2.0;
        let d_internal = (c.k_v_to_target_out + c.k_v_to_target_in)
            - (c.k_v_to_current_out + c.k_v_to_current_in);
        let d_expected = c.k_v_in * (c.sigma_tot_target_out - c.sigma_tot_current_out)
            + c.k_v_out * (c.sigma_tot_target_in - c.sigma_tot_current_in)
            + 2.0 * c.k_v_out * c.k_v_in;
        d_internal / m - resolution * d_expected / (m * m)
    }
}

fn modularity_total_quality(
    resolution: f64,
    data: &GraphData,
    partition: &crate::leiden_impl::partition::Partition,
) -> f64 {
    let n = data.node_count();
    let m = data.total_weight();
    if m == 0.0 {
        return 0.0;
    }

    let num_comms = partition.num_communities();

    if !data.is_directed() {
        let mut sigma_tot: Vec<f64> = vec![0.0; num_comms];
        let mut e_c: Vec<f64> = vec![0.0; num_comms];

        for node in 0..n {
            let comm = partition.community_of(node);
            if comm >= num_comms {
                continue;
            }
            sigma_tot[comm] += data.degree_of(node);
            for (neighbor, weight) in data.neighbors(node) {
                if neighbor >= node && partition.community_of(neighbor) == comm {
                    e_c[comm] += weight;
                }
            }
        }

        let two_m = 2.0 * m;
        let mut q = 0.0;
        for c in 0..num_comms {
            q += e_c[c] / m - resolution * (sigma_tot[c] / two_m).powi(2);
        }
        q
    } else {
        let mut sigma_tot_out: Vec<f64> = vec![0.0; num_comms];
        let mut sigma_tot_in: Vec<f64> = vec![0.0; num_comms];
        let mut e_c: Vec<f64> = vec![0.0; num_comms];

        for node in 0..n {
            let comm = partition.community_of(node);
            if comm >= num_comms {
                continue;
            }
            sigma_tot_out[comm] += data.out_degree_of(node);
            sigma_tot_in[comm] += data.in_degree_of(node);
            for (neighbor, weight) in data.out_neighbors(node) {
                if partition.community_of(neighbor) == comm {
                    e_c[comm] += weight;
                }
            }
        }

        let mut q = 0.0;
        for c in 0..num_comms {
            q += e_c[c] / m - resolution * sigma_tot_out[c] * sigma_tot_in[c] / (m * m);
        }
        q
    }
}

impl QualityFunction for Modularity {
    #[inline]
    fn delta_move_from_components(&self, c: &MoveComponents) -> f64 {
        modularity_delta(self.resolution, c)
    }

    fn total_quality(&self, data: &GraphData, partition: &crate::leiden_impl::partition::Partition) -> f64 {
        modularity_total_quality(self.resolution, data, partition)
    }
}

/// CPM (Constant Potts Model): H = Σ_c [e_c - γ * n_c * (n_c - 1) / 2]
pub struct CPM {
    /// Resolution parameter γ.
    pub resolution: f64,
}

impl CPM {
    /// Create a new CPM with the given resolution parameter.
    #[must_use = "constructor returns a new instance"]
    pub fn new(resolution: f64) -> Self {
        Self { resolution }
    }
}

impl QualityFunction for CPM {
    #[inline]
    fn delta_move_from_components(&self, c: &MoveComponents) -> f64 {
        (c.k_v_to_target_out + c.k_v_to_target_in)
            - (c.k_v_to_current_out + c.k_v_to_current_in)
            - self.resolution * c.node_weight * (c.n_target - c.n_current + c.node_weight)
    }

    fn total_quality(&self, data: &GraphData, partition: &crate::leiden_impl::partition::Partition) -> f64 {
        let n = data.node_count();
        let num_comms = partition.num_communities();
        let mut e_c: Vec<f64> = vec![0.0; num_comms];
        let mut n_c: Vec<f64> = vec![0.0; num_comms];

        let directed = data.is_directed();
        for node in 0..n {
            let comm = partition.community_of(node);
            if comm >= num_comms {
                continue;
            }
            n_c[comm] += data.node_weight(node);
            if directed {
                for (neighbor, weight) in data.out_neighbors(node) {
                    if partition.community_of(neighbor) == comm {
                        e_c[comm] += weight;
                    }
                }
            } else {
                for (neighbor, weight) in data.neighbors(node) {
                    if neighbor >= node && partition.community_of(neighbor) == comm {
                        e_c[comm] += weight;
                    }
                }
            }
        }

        let mut h = 0.0;
        for c in 0..num_comms {
            h += e_c[c] - self.resolution * n_c[c] * (n_c[c] - 1.0) / 2.0;
        }
        h
    }
}

/// RBConfiguration: Reichardt-Bornholdt with configuration model null.
///
/// Q = Σ_c [e_c - γ * K_c² / (4m)]
///
/// Mathematically equivalent to `Modularity::with_resolution(γ)`.
/// Provided for API compatibility with the leidenalg Python library.
pub struct RBConfiguration {
    /// Resolution parameter γ.
    pub resolution: f64,
}

impl RBConfiguration {
    /// Create a new RBConfiguration with default resolution (1.0).
    #[must_use = "constructor returns a new instance"]
    pub fn new() -> Self {
        Self { resolution: 1.0 }
    }

    /// Create a new RBConfiguration with a custom resolution parameter.
    #[must_use = "constructor returns a new instance"]
    pub fn with_resolution(resolution: f64) -> Self {
        Self { resolution }
    }
}

impl Default for RBConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityFunction for RBConfiguration {
    #[inline]
    fn delta_move_from_components(&self, c: &MoveComponents) -> f64 {
        modularity_delta(self.resolution, c)
    }

    fn total_quality(&self, data: &GraphData, partition: &crate::leiden_impl::partition::Partition) -> f64 {
        modularity_total_quality(self.resolution, data, partition)
    }
}

/// RBER: Reichardt-Bornholdt with Erdős-Rényi null model.
///
/// Q = Σ_c [e_c - γ * p * n_c * (n_c - 1) / 2]
///
/// Where p = 2m / (N*(N-1)) is the graph density and N is the total node weight.
pub struct RBER {
    /// Resolution parameter γ.
    pub resolution: f64,
}

impl RBER {
    /// Create a new RBER with the given resolution parameter.
    #[must_use = "constructor returns a new instance"]
    pub fn new(resolution: f64) -> Self {
        Self { resolution }
    }
}

impl QualityFunction for RBER {
    #[inline]
    fn delta_move_from_components(&self, c: &MoveComponents) -> f64 {
        let total_n = c.total_node_weight;
        if total_n <= 1.0 || c.two_m == 0.0 {
            return 0.0;
        }
        let p = c.two_m / (total_n * (total_n - 1.0));
        (c.k_v_to_target_out + c.k_v_to_target_in)
            - (c.k_v_to_current_out + c.k_v_to_current_in)
            - self.resolution * p * c.node_weight * (c.n_target - c.n_current + c.node_weight)
    }

    fn total_quality(&self, data: &GraphData, partition: &crate::leiden_impl::partition::Partition) -> f64 {
        let n = data.node_count();
        let m = data.total_weight();
        if n <= 1 || m == 0.0 {
            return 0.0;
        }

        let total_n = data.total_node_weight();
        if total_n <= 1.0 {
            return 0.0;
        }
        let p = 2.0 * m / (total_n * (total_n - 1.0));

        let num_comms = partition.num_communities();
        let mut e_c: Vec<f64> = vec![0.0; num_comms];
        let mut n_c: Vec<f64> = vec![0.0; num_comms];

        let directed = data.is_directed();
        for node in 0..n {
            let comm = partition.community_of(node);
            if comm >= num_comms {
                continue;
            }
            n_c[comm] += data.node_weight(node);
            if directed {
                for (neighbor, weight) in data.out_neighbors(node) {
                    if partition.community_of(neighbor) == comm {
                        e_c[comm] += weight;
                    }
                }
            } else {
                for (neighbor, weight) in data.neighbors(node) {
                    if neighbor >= node && partition.community_of(neighbor) == comm {
                        e_c[comm] += weight;
                    }
                }
            }
        }

        let mut q = 0.0;
        for c in 0..num_comms {
            q += e_c[c] - self.resolution * p * n_c[c] * (n_c[c] - 1.0) / 2.0;
        }
        q
    }
}

#[cfg(test)]
#[path = "quality_tests.rs"]
mod tests;
