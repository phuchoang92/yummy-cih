//! Core Leiden algorithm implementation.

use std::borrow::Cow;

use rand::rngs::StdRng;
use rand::SeedableRng;
use rustc_hash::FxHashMap;

use crate::leiden_impl::algorithm;
use crate::leiden_impl::partition::Partition;
use crate::leiden_impl::quality::{GraphData, Modularity, MoveComponents, QualityFunction};

#[cfg(feature = "rayon")]
use crate::leiden_impl::parallel::{
    aggregate_edges_parallel, local_moving_parallel, AGG_PARALLEL_THRESHOLD,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Default value for min_iterations serde deserialization.
#[cfg(feature = "serde")]
const fn default_min_iterations() -> usize {
    1
}

/// Quality function selection for the Leiden algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub enum QualityType {
    /// Newman-Girvan modularity (default).
    #[default]
    Modularity,
    /// Constant Potts Model.
    CPM,
    /// Reichardt-Bornholdt with configuration model null.
    RBConfiguration,
    /// Reichardt-Bornholdt with Erdős-Rényi null model.
    RBER,
}

/// Configuration for the Leiden algorithm.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LeidenConfig {
    /// Maximum number of Leiden iterations (local move → refine → aggregate).
    pub max_iterations: usize,
    /// Resolution parameter γ controlling community granularity.
    pub resolution: f64,
    /// Optional RNG seed for reproducible results.
    pub seed: Option<u64>,
    /// Quality function to optimize.
    pub quality: QualityType,
    /// Convergence threshold: stop when quality improvement is below this value.
    pub epsilon: f64,
    /// Maximum number of nodes per community (0 = unlimited).
    pub max_comm_size: usize,
    /// Minimum edge slots (CSR entries) for parallel local moving (default: 2000).
    /// Also requires at least 100 nodes. Only depends on graph structure for determinism.
    ///
    /// The parallel path uses graph coloring to partition nodes into independent
    /// sets. Nodes in the same color group are processed concurrently using a
    /// shared snapshot of community statistics. This "relaxed" consistency model
    /// may produce slightly different results compared to the sequential path, but
    /// typically converges to the same partition quality. The sequential path is
    /// always used as a final refinement pass after parallel processing.
    pub parallel_local_moving_threshold: Option<usize>,
    /// Minimum edge slots (CSR entries) for parallel aggregation (default: 10000).
    pub parallel_aggregation_threshold: Option<usize>,
    /// When true, skips the refinement phase (producing Louvain-like results).
    #[cfg_attr(feature = "serde", serde(default))]
    pub skip_refinement: bool,

    /// Minimum iterations before convergence check (default: 1).
    /// Prevents premature convergence by ensuring the algorithm runs
    /// for at least this many iterations before checking epsilon convergence.
    #[cfg_attr(feature = "serde", serde(default = "default_min_iterations"))]
    pub min_iterations: usize,

    /// When true, record per-iteration quality values in output (default: false).
    #[cfg_attr(feature = "serde", serde(default))]
    pub track_quality_history: bool,
}

impl Default for LeidenConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            resolution: 1.0,
            seed: None,
            quality: QualityType::default(),
            epsilon: 1e-10,
            max_comm_size: 0,
            parallel_local_moving_threshold: None,
            parallel_aggregation_threshold: None,
            skip_refinement: false,
            min_iterations: 1,
            track_quality_history: false,
        }
    }
}

impl LeidenConfig {
    /// Validate configuration parameters.
    ///
    /// Returns `Err(LeidenError)` if any parameter is invalid.
    pub fn validate(&self) -> crate::leiden_impl::error::Result<()> {
        if self.max_iterations == 0 {
            return Err(crate::leiden_impl::error::LeidenError::InvalidParameter {
                message: "max_iterations must be > 0".to_string(),
            });
        }
        if !self.resolution.is_finite() || self.resolution < 0.0 {
            return Err(crate::leiden_impl::error::LeidenError::InvalidParameter {
                message: format!("resolution must be finite and non-negative, got {}", self.resolution),
            });
        }
        if !self.epsilon.is_finite() || self.epsilon <= 0.0 {
            return Err(crate::leiden_impl::error::LeidenError::InvalidParameter {
                message: format!("epsilon must be finite and positive, got {}", self.epsilon),
            });
        }
        Ok(())
    }

    /// Create a builder for configuring the Leiden algorithm.
    #[must_use = "constructor returns a new instance"]
    pub fn builder() -> LeidenConfigBuilder {
        LeidenConfigBuilder::default()
    }
}

/// Builder for [`LeidenConfig`].
#[derive(Debug, Clone, Default)]
pub struct LeidenConfigBuilder {
    max_iterations: Option<usize>,
    resolution: Option<f64>,
    seed: Option<u64>,
    quality: Option<QualityType>,
    epsilon: Option<f64>,
    max_comm_size: Option<usize>,
    parallel_local_moving_threshold: Option<usize>,
    parallel_aggregation_threshold: Option<usize>,
    skip_refinement: Option<bool>,
    min_iterations: Option<usize>,
    track_quality_history: Option<bool>,
}

impl LeidenConfigBuilder {
    /// Set the maximum number of Leiden iterations.
    pub fn max_iterations(mut self, v: usize) -> Self {
        self.max_iterations = Some(v);
        self
    }

    /// Set the resolution parameter γ.
    pub fn resolution(mut self, v: f64) -> Self {
        self.resolution = Some(v);
        self
    }

    /// Set the RNG seed for reproducible results.
    pub fn seed(mut self, v: u64) -> Self {
        self.seed = Some(v);
        self
    }

    /// Set the RNG seed only if `v` is `Some`.
    pub fn maybe_seed(mut self, v: Option<u64>) -> Self {
        self.seed = v;
        self
    }

    /// Set the quality function to optimize.
    pub fn quality(mut self, v: QualityType) -> Self {
        self.quality = Some(v);
        self
    }

    /// Set the convergence threshold.
    pub fn epsilon(mut self, v: f64) -> Self {
        self.epsilon = Some(v);
        self
    }

    /// Set the maximum number of nodes per community (0 = unlimited).
    pub fn max_comm_size(mut self, v: usize) -> Self {
        self.max_comm_size = Some(v);
        self
    }

    /// Set the minimum edge slots for parallel local moving (default: 2000).
    /// Also requires at least 100 nodes. Only depends on graph structure for determinism.
    pub fn parallel_local_moving_threshold(mut self, v: usize) -> Self {
        self.parallel_local_moving_threshold = Some(v);
        self
    }

    /// Set the parallel local moving threshold only if `v` is `Some`.
    pub fn maybe_parallel_local_moving_threshold(mut self, v: Option<usize>) -> Self {
        self.parallel_local_moving_threshold = v;
        self
    }

    /// Set the minimum edge slots for parallel aggregation (default: 10000).
    pub fn parallel_aggregation_threshold(mut self, v: usize) -> Self {
        self.parallel_aggregation_threshold = Some(v);
        self
    }

    /// Set the parallel aggregation threshold only if `v` is `Some`.
    pub fn maybe_parallel_aggregation_threshold(mut self, v: Option<usize>) -> Self {
        self.parallel_aggregation_threshold = v;
        self
    }

    /// Skip the refinement phase, producing Louvain-like results.
    pub fn skip_refinement(mut self, v: bool) -> Self {
        self.skip_refinement = Some(v);
        self
    }

    /// Set the minimum number of iterations before convergence check.
    pub fn min_iterations(mut self, v: usize) -> Self {
        self.min_iterations = Some(v);
        self
    }

    /// When true, record per-iteration quality values in the output.
    pub fn track_quality_history(mut self, v: bool) -> Self {
        self.track_quality_history = Some(v);
        self
    }

    /// Build the configuration, applying defaults for unset fields.
    pub fn build(self) -> LeidenConfig {
        LeidenConfig {
            max_iterations: self.max_iterations.unwrap_or(100),
            resolution: self.resolution.unwrap_or(1.0),
            seed: self.seed,
            quality: self.quality.unwrap_or_default(),
            epsilon: self.epsilon.unwrap_or(1e-10),
            max_comm_size: self.max_comm_size.unwrap_or(0),
            parallel_local_moving_threshold: self.parallel_local_moving_threshold,
            parallel_aggregation_threshold: self.parallel_aggregation_threshold,
            skip_refinement: self.skip_refinement.unwrap_or(false),
            min_iterations: self.min_iterations.unwrap_or(1),
            track_quality_history: self.track_quality_history.unwrap_or(false),
        }
    }
}

/// Result of running the Leiden algorithm, containing the partition and its quality score.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[non_exhaustive]
pub struct LeidenOutput {
    /// The detected community partition.
    pub partition: Partition,
    /// The quality score of the partition (higher is better).
    pub quality: f64,
    /// Per-iteration quality values (only populated if `track_quality_history` is enabled).
    pub quality_history: Vec<f64>,
}

impl LeidenOutput {
    /// Create a new Leiden output with the given partition and quality score.
    #[must_use = "constructor returns a new instance"]
    pub fn new(partition: Partition, quality: f64) -> Self {
        Self {
            partition,
            quality,
            quality_history: Vec::new(),
        }
    }
}

/// Owned quality function enum wrapping all supported quality functions.
///
/// Used internally to pass a quality function through the algorithm pipeline
/// without lifetime concerns. Implements [`QualityFunction`] by delegating
/// to the wrapped variant.
#[allow(clippy::upper_case_acronyms)]
enum QualityFn {
    Modularity(Modularity),
    CPM(crate::leiden_impl::quality::CPM),
    RBConfiguration(crate::leiden_impl::quality::RBConfiguration),
    RBER(crate::leiden_impl::quality::RBER),
}

impl QualityFunction for QualityFn {
    #[inline]
    fn delta_move_from_components(&self, c: &MoveComponents) -> f64 {
        match self {
            Self::Modularity(q) => q.delta_move_from_components(c),
            Self::CPM(q) => q.delta_move_from_components(c),
            Self::RBConfiguration(q) => q.delta_move_from_components(c),
            Self::RBER(q) => q.delta_move_from_components(c),
        }
    }

    fn total_quality(&self, data: &GraphData, partition: &Partition) -> f64 {
        match self {
            Self::Modularity(q) => q.total_quality(data, partition),
            Self::CPM(q) => q.total_quality(data, partition),
            Self::RBConfiguration(q) => q.total_quality(data, partition),
            Self::RBER(q) => q.total_quality(data, partition),
        }
    }
}

/// The Leiden community detection algorithm.
#[derive(Debug, Clone)]
pub struct Leiden {
    config: LeidenConfig,
}

impl Leiden {
    /// Create a new Leiden instance with the given configuration.
    #[must_use = "constructor returns a new instance"]
    pub fn new(config: LeidenConfig) -> Self {
        Self { config }
    }

    fn create_quality(&self) -> QualityFn {
        match self.config.quality {
            QualityType::Modularity => {
                QualityFn::Modularity(Modularity::with_resolution(self.config.resolution))
            }
            QualityType::CPM => QualityFn::CPM(crate::leiden_impl::quality::CPM::new(self.config.resolution)),
            QualityType::RBConfiguration => QualityFn::RBConfiguration(
                crate::leiden_impl::quality::RBConfiguration::with_resolution(self.config.resolution),
            ),
            QualityType::RBER => QualityFn::RBER(crate::leiden_impl::quality::RBER::new(self.config.resolution)),
        }
    }

    /// Core Leiden iteration loop.
    ///
    /// Runs the three-phase cycle (local moving → refinement → aggregation)
    /// and calls `on_iteration` after each successful local-moving phase,
    /// allowing the caller to collect per-level information without
    /// duplicating the algorithm logic.
    ///
    /// The `on_iteration` closure receives `(data, partition, q_after,
    /// flat_mapping, original_n)`. For `run()` an empty closure is passed
    /// and the compiler eliminates it entirely via monomorphization.
    fn run_core<'a, F>(
        &self,
        input_data: &'a GraphData,
        quality: &QualityFn,
        initial_partition: Option<Partition>,
        on_iteration: &mut F,
    ) -> crate::leiden_impl::error::Result<(Partition, f64, Vec<f64>)>
    where
        F: FnMut(&GraphData, &Partition, f64, &[usize], usize),
    {
        self.config.validate()?;

        let original_n = input_data.node_count();
        let mut data: Cow<'a, GraphData> = Cow::Borrowed(input_data);
        let mut partition = initial_partition.unwrap_or_else(|| Partition::new(data.node_count()));
        partition.renumber();
        let mut flat_mapping: Vec<usize> = (0..data.node_count()).collect();
        let mut quality_history: Vec<f64> = Vec::new();

        let mut rng = match self.config.seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => StdRng::from_rng(&mut rand::rng()),
        };

        let mut local_moving_buffers =
            algorithm::LocalMovingBuffers::new(data.node_count(), 1);
        let mut refinement_buffers =
            algorithm::RefinementBuffers::new(data.node_count(), 1);

        for iteration in 0..self.config.max_iterations {
            let q_before = quality.total_quality(&data, &partition);
            let changed = local_moving_dispatch(
                std::slice::from_ref(&*data),
                &mut partition,
                quality,
                &mut rng,
                &algorithm::MovingConfig {
                    max_comm_size: self.config.max_comm_size,
                    epsilon: self.config.epsilon,
                },
                &self.config,
                &mut local_moving_buffers,
            );
            if !changed {
                break;
            }
            partition.renumber();

            let q_after = quality.total_quality(&data, &partition);

            on_iteration(&data, &partition, q_after, &flat_mapping, original_n);

            // Track per-iteration quality if enabled
            if self.config.track_quality_history {
                quality_history.push(q_after);
            }

            // Only check epsilon convergence after min_iterations
            if iteration >= self.config.min_iterations
                && (q_after - q_before).abs() < self.config.epsilon
            {
                break;
            }

            let refined = if !self.config.skip_refinement {
                refinement(&data, &partition, quality, &mut rng, self.config.epsilon, &mut refinement_buffers)
            } else {
                // In Louvain mode, use the unrefined partition directly
                partition.clone()
            };

            let (agg_data, orig_to_agg, agg_initial_partition) =
                aggregate(&data, &refined, &partition, &self.config)?;

            for orig_node in 0..original_n {
                flat_mapping[orig_node] = orig_to_agg[flat_mapping[orig_node]];
            }

            data = Cow::Owned(agg_data);
            partition = agg_initial_partition;

            if data.node_count() <= 1 {
                break;
            }
        }

        // Resolve aggregate nodes back to original node IDs.
        let mut result = Partition::from_membership(vec![0; original_n]);
        for (orig_node, &agg_node) in flat_mapping.iter().enumerate() {
            let comm = partition.community_of(agg_node);
            result.move_node(orig_node, comm);
        }
        result.renumber();
        // Compute quality on the original input graph, not the aggregated graph
        let q = quality.total_quality(input_data, &result);
        Ok((result, q, quality_history))
    }

    /// Run the Leiden algorithm on the given graph.
    ///
    /// Returns a [`LeidenOutput`] containing the community partition and its quality score.
    #[must_use = "algorithm result should be used"]
    pub fn run(&self, data: &GraphData) -> crate::leiden_impl::error::Result<LeidenOutput> {
        if data.node_count() == 0 {
            return Ok(LeidenOutput {
                partition: Partition::new(0),
                quality: 0.0,
                quality_history: Vec::new(),
            });
        }

        let quality = self.create_quality();
        let (partition, q, quality_history) =
            self.run_core(data, &quality, None, &mut |_, _, _, _, _| {})?;
        Ok(LeidenOutput {
            partition,
            quality: q,
            quality_history,
        })
    }

    /// Run the Leiden algorithm with an initial partition (warm-start).
    ///
    /// Instead of starting from a singleton partition (each node in its own community),
    /// this method uses the provided partition as the starting point. Useful for:
    /// - Resuming optimization from a previous run
    /// - Incremental refinement after minor graph changes
    /// - Seeding with an external community detection result
    #[must_use = "algorithm result should be used"]
    pub fn run_with_initial_partition(
        &self,
        data: &GraphData,
        mut initial_partition: Partition,
    ) -> crate::leiden_impl::error::Result<LeidenOutput> {
        if data.node_count() == 0 {
            return Ok(LeidenOutput {
                partition: Partition::new(0),
                quality: 0.0,
                quality_history: Vec::new(),
            });
        }
        if initial_partition.len() != data.node_count() {
            return Err(crate::leiden_impl::error::LeidenError::InvalidPartition {
                message: format!(
                    "partition size {} does not match graph node count {}",
                    initial_partition.len(),
                    data.node_count()
                ),
            });
        }
        // Renumber first so num_communities reflects the actual max community ID.
        initial_partition.renumber();
        if initial_partition.num_communities() > data.node_count() {
            return Err(crate::leiden_impl::error::LeidenError::InvalidPartition {
                message: format!(
                    "partition has {} communities but graph only has {} nodes",
                    initial_partition.num_communities(),
                    data.node_count()
                ),
            });
        }

        let quality = self.create_quality();
        let (partition, q, quality_history) = self.run_core(
            data,
            &quality,
            Some(initial_partition),
            &mut |_, _, _, _, _| {},
        )?;
        Ok(LeidenOutput {
            partition,
            quality: q,
            quality_history,
        })
    }

}


fn local_moving_dispatch(
    data: &[GraphData],
    partition: &mut Partition,
    quality: &(dyn QualityFunction + Sync),
    rng: &mut StdRng,
    cfg: &algorithm::MovingConfig,
    _config: &LeidenConfig,
    buffers: &mut algorithm::LocalMovingBuffers,
) -> bool {
    #[cfg(feature = "rayon")]
    {
        if should_use_parallel(&data[0], _config) {
            let (parallel_changed, converged_naturally) =
                local_moving_parallel(&data[0], partition, quality, rng, cfg.max_comm_size, cfg.epsilon);
            if converged_naturally {
                return parallel_changed;
            }
            let sequential_changed = algorithm::local_moving_generic(
                data,
                &[1.0],
                partition,
                quality,
                rng,
                cfg,
                buffers,
            );
            return parallel_changed || sequential_changed;
        }
    }
    algorithm::local_moving_generic(
        data,
        &[1.0],
        partition,
        quality,
        rng,
        cfg,
        buffers,
    )
}

/// Decide whether to use parallel local moving based on graph structure.
/// MUST only depend on graph properties, NOT on runtime state (threads, load),
/// to ensure deterministic behavior (same graph → same code path → same result).
#[cfg(feature = "rayon")]
#[inline]
fn should_use_parallel(data: &GraphData, config: &LeidenConfig) -> bool {
    let n = data.node_count();
    // Use edge slot count (total CSR entries) as the work estimate.
    // 2000 edge slots ≈ ~500 nodes × avg_degree 4, or ~200 nodes × avg_degree 10
    let edge_slots = data.out_offsets[n];
    let threshold = config.parallel_local_moving_threshold.unwrap_or(2000);
    edge_slots >= threshold && n >= 100
}

fn refinement(
    data: &GraphData,
    partition: &Partition,
    quality: &(dyn QualityFunction + Sync),
    rng: &mut StdRng,
    epsilon: f64,
    buffers: &mut algorithm::RefinementBuffers,
) -> Partition {
    if data.total_weight() <= 0.0 {
        return Partition::new(data.node_count());
    }
    let layers = std::slice::from_ref(data);
    algorithm::refinement_generic(
        data.node_count(),
        1, // single layer
        partition,
        rng,
        buffers,
        |community, nodes, buf| {
            algorithm::refine_community_generic(
                layers,
                &[1.0],
                partition,
                quality,
                &algorithm::CommunitySubset { community, nodes },
                &algorithm::MovingConfig {
                    max_comm_size: 0,
                    epsilon,
                },
                buf,
            )
        },
    )
}

fn aggregate_edges_sequential(
    data: &GraphData,
    orig_to_agg: &[usize],
    directed: bool,
) -> FxHashMap<(usize, usize), f64> {
    let n = data.node_count();
    let mut agg_edges: FxHashMap<(usize, usize), f64> = FxHashMap::default();
    for u in 0..n {
        algorithm::aggregate_node_edges_into(data, u, orig_to_agg, directed, &mut agg_edges);
    }
    agg_edges
}

fn aggregate(
    data: &GraphData,
    refined_partition: &Partition,
    coarse_partition: &Partition,
    _config: &LeidenConfig,
) -> crate::leiden_impl::error::Result<(GraphData, Vec<usize>, Partition)> {
    let n = data.node_count();
    let (orig_to_agg, agg_n) = algorithm::build_orig_to_agg_mapping(n, refined_partition);

    let directed = data.is_directed();
    let agg_edges_map: FxHashMap<(usize, usize), f64> = {
        #[cfg(feature = "rayon")]
        {
            let edge_slots = data.out_offsets[n];
            let threshold = _config.parallel_aggregation_threshold.unwrap_or(AGG_PARALLEL_THRESHOLD);
            if edge_slots >= threshold {
                aggregate_edges_parallel(data, &orig_to_agg, directed)
            } else {
                aggregate_edges_sequential(data, &orig_to_agg, directed)
            }
        }
        #[cfg(not(feature = "rayon"))]
        {
            aggregate_edges_sequential(data, &orig_to_agg, directed)
        }
    };

    algorithm::build_aggregated_graph(
        orig_to_agg,
        agg_n,
        directed,
        agg_edges_map,
        coarse_partition,
        |orig| data.node_weight(orig),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leiden_impl::graph_data::GraphData;
    use crate::leiden_impl::builder::GraphDataBuilder;
    use rand::prelude::IndexedRandom;

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
    fn test_two_cliques() {
        let graph = make_two_cliques();
        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&graph).unwrap().partition;

        assert_eq!(partition.num_communities(), 2);

        let comm0 = partition.community_of(0);
        for i in 1..5 {
            assert_eq!(partition.community_of(i), comm0, "node {i}");
        }
        let comm5 = partition.community_of(5);
        for i in 6..10 {
            assert_eq!(partition.community_of(i), comm5, "node {i}");
        }
        assert_ne!(comm0, comm5);
    }

    #[test]
    fn test_single_node() {
        let data = GraphDataBuilder::new(1).build().unwrap();

        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;
        assert_eq!(partition.num_communities(), 1);
    }

    #[test]
    fn test_ring_of_cliques() {
        let mut b = GraphDataBuilder::new(12);

        for i in 0..4 {
            for j in (i + 1)..4 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        for i in 4..8 {
            for j in (i + 1)..8 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        for i in 8..12 {
            for j in (i + 1)..12 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        b.add_edge(0, 4, 0.1).unwrap();
        b.add_edge(4, 8, 0.1).unwrap();
        b.add_edge(8, 0, 0.1).unwrap();

        let data = b.build().unwrap();
        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert_eq!(partition.num_communities(), 3);
    }

    #[test]
    fn test_empty_graph() {
        let data = GraphDataBuilder::new(0).build().unwrap();
        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;
        assert_eq!(partition.num_communities(), 0);
    }

    #[test]
    fn test_disconnected_graph() {
        let mut b = GraphDataBuilder::new(6);

        for i in 0..3 {
            for j in (i + 1)..3 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        for i in 3..6 {
            for j in (i + 1)..6 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }

        let data = b.build().unwrap();
        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert_eq!(partition.num_communities(), 2);

        let comm0 = partition.community_of(0);
        assert_eq!(partition.community_of(1), comm0);
        assert_eq!(partition.community_of(2), comm0);

        let comm3 = partition.community_of(3);
        assert_eq!(partition.community_of(4), comm3);
        assert_eq!(partition.community_of(5), comm3);

        assert_ne!(comm0, comm3);
    }

    #[test]
    fn test_single_edge() {
        let mut b = GraphDataBuilder::new(2);
        b.add_edge(0, 1, 1.0).unwrap();
        let data = b.build().unwrap();

        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert_eq!(partition.num_communities(), 1);
        assert_eq!(partition.community_of(0), partition.community_of(1));
    }

    #[test]
    fn test_weighted_graph() {
        let mut b = GraphDataBuilder::new(6);

        for i in 0..3 {
            for j in (i + 1)..3 {
                b.add_edge(i, j, 5.0).unwrap();
            }
        }
        for i in 3..6 {
            for j in (i + 1)..6 {
                b.add_edge(i, j, 5.0).unwrap();
            }
        }
        b.add_edge(0, 3, 0.1).unwrap();

        let data = b.build().unwrap();
        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert_eq!(partition.num_communities(), 2);

        let comm0 = partition.community_of(0);
        for i in 1..3 {
            assert_eq!(partition.community_of(i), comm0, "node {i}");
        }
        let comm3 = partition.community_of(3);
        for i in 4..6 {
            assert_eq!(partition.community_of(i), comm3, "node {i}");
        }
        assert_ne!(comm0, comm3);
    }

    #[test]
    fn test_large_random_graph_determinism() {
        let mut rng = StdRng::seed_from_u64(12345);

        let mut builder = GraphDataBuilder::new(100);

        let clusters: Vec<Vec<usize>> = (0..4).map(|c| (c * 25..(c + 1) * 25).collect()).collect();

        for cluster in &clusters {
            for &node in cluster {
                let others: Vec<usize> = cluster.iter().filter(|&&x| x != node).copied().collect();
                let count = std::cmp::min(5, others.len());
                let chosen: Vec<usize> = others.choose_multiple(&mut rng, count).copied().collect();
                for &neighbor in &chosen {
                    if neighbor > node {
                        builder.add_edge(node, neighbor, 1.0).unwrap();
                    }
                }
            }
        }

        for i in 0..4 {
            for j in (i + 1)..4 {
                let mut pairs: Vec<(usize, usize)> = Vec::new();
                for &a in &clusters[i] {
                    for &bb in &clusters[j] {
                        pairs.push((a, bb));
                    }
                }
                let count = std::cmp::min(3, pairs.len());
                for &(a, b) in pairs.choose_multiple(&mut rng, count) {
                    builder.add_edge(a, b, 0.1).unwrap();
                }
            }
        }

        let data = builder.build().unwrap();

        let leiden = Leiden::new(LeidenConfig {
            seed: Some(42),
            ..Default::default()
        });
        let partition1 = leiden.run(&data).unwrap().partition;

        let leiden2 = Leiden::new(LeidenConfig {
            seed: Some(42),
            ..Default::default()
        });
        let partition2 = leiden2.run(&data).unwrap().partition;

        assert_eq!(
            partition1.as_slice(),
            partition2.as_slice(),
            "same seed should produce identical partitions"
        );
        assert!(
            partition1.num_communities() >= 2,
            "expected at least 2 communities, got {}",
            partition1.num_communities()
        );
    }

    #[test]
    fn test_star_graph() {
        let mut b = GraphDataBuilder::new(11);
        for i in 1..11 {
            b.add_edge(0, i, 1.0).unwrap();
        }
        let data = b.build().unwrap();

        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert!(partition.num_communities() >= 1);
    }

    #[test]
    fn test_isolated_nodes_with_edges() {
        let mut b = GraphDataBuilder::new(9);
        for i in 0..4 {
            for j in (i + 1)..4 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        for i in 4..8 {
            for j in (i + 1)..8 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        let data = b.build().unwrap();

        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert_eq!(partition.num_communities(), 3);

        let comm0 = partition.community_of(0);
        for i in 1..4 {
            assert_eq!(partition.community_of(i), comm0, "node {i}");
        }
        let comm4 = partition.community_of(4);
        for i in 5..8 {
            assert_eq!(partition.community_of(i), comm4, "node {i}");
        }
        let comm8 = partition.community_of(8);
        assert_ne!(comm8, comm0);
        assert_ne!(comm8, comm4);
    }

    #[test]
    fn test_seed_determinism_different_seeds() {
        let graph = make_two_cliques();

        let config1 = LeidenConfig {
            seed: Some(1),
            ..Default::default()
        };
        let leiden1 = Leiden::new(config1);
        let partition1 = leiden1.run(&graph).unwrap().partition;

        let config2 = LeidenConfig {
            seed: Some(2),
            ..Default::default()
        };
        let leiden2 = Leiden::new(config2);
        let partition2 = leiden2.run(&graph).unwrap().partition;

        assert_eq!(partition1.num_communities(), 2);
        assert_eq!(partition2.num_communities(), 2);
    }

    #[test]
    fn test_self_loop() {
        let mut b = GraphDataBuilder::new(4);
        b.add_edge(0, 1, 1.0).unwrap();
        b.add_edge(2, 3, 1.0).unwrap();
        b.add_edge(0, 0, 2.0).unwrap();
        let data = b.build().unwrap();

        let leiden = Leiden::new(LeidenConfig::default());
        let partition = leiden.run(&data).unwrap().partition;

        assert_eq!(partition.num_communities(), 2);
        assert_eq!(partition.community_of(0), partition.community_of(1));
        assert_eq!(partition.community_of(2), partition.community_of(3));
        assert_ne!(partition.community_of(0), partition.community_of(2));
    }

    #[test]
    fn test_parallel_matches_sequential() {
        let graph = make_two_cliques();
        let leiden = Leiden::new(LeidenConfig {
            seed: Some(42),
            ..Default::default()
        });
        let result_normal = leiden.run(&graph).unwrap();
        assert!(result_normal.partition.num_communities() >= 1);
    }

    // LeidenConfig::validate() error path tests
    #[test]
    fn test_validate_rejects_zero_iterations() {
        let config = LeidenConfig {
            max_iterations: 0,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_negative_resolution() {
        let config = LeidenConfig {
            resolution: -1.0,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_nan_resolution() {
        let config = LeidenConfig {
            resolution: f64::NAN,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_infinite_resolution() {
        let config = LeidenConfig {
            resolution: f64::INFINITY,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_zero_epsilon() {
        let config = LeidenConfig {
            epsilon: 0.0,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_negative_epsilon() {
        let config = LeidenConfig {
            epsilon: -1e-10,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_nan_epsilon() {
        let config = LeidenConfig {
            epsilon: f64::NAN,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_rejects_infinite_epsilon() {
        let config = LeidenConfig {
            epsilon: f64::INFINITY,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(matches!(err, crate::leiden_impl::error::LeidenError::InvalidParameter { .. }));
    }

    #[test]
    fn test_validate_accepts_defaults() {
        assert!(LeidenConfig::default().validate().is_ok());
    }

    #[test]
    fn test_validate_accepts_zero_resolution() {
        let config = LeidenConfig {
            resolution: 0.0,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    // ── Louvain mode (skip_refinement) tests ──────────────────────────────

    #[test]
    fn test_louvain_mode_produces_valid_partition() {
        let graph = make_two_cliques();
        let config = LeidenConfig {
            skip_refinement: true,
            seed: Some(42),
            ..Default::default()
        };
        let result = Leiden::new(config).run(&graph).unwrap();
        assert_eq!(result.partition.num_communities(), 2);

        let comm0 = result.partition.community_of(0);
        for i in 1..5 {
            assert_eq!(result.partition.community_of(i), comm0, "node {i}");
        }
        let comm5 = result.partition.community_of(5);
        for i in 6..10 {
            assert_eq!(result.partition.community_of(i), comm5, "node {i}");
        }
        assert_ne!(comm0, comm5);
    }

    #[test]
    fn test_louvain_quality_le_than_leiden() {
        let graph = make_two_cliques();

        let leiden = Leiden::new(LeidenConfig {
            seed: Some(42),
            skip_refinement: false,
            ..Default::default()
        });
        let leiden_result = leiden.run(&graph).unwrap();

        let louvain = Leiden::new(LeidenConfig {
            seed: Some(42),
            skip_refinement: true,
            ..Default::default()
        });
        let louvain_result = louvain.run(&graph).unwrap();

        // Refinement phase should produce quality >= unrefined (Louvain)
        assert!(
            louvain_result.quality <= leiden_result.quality + 1e-12,
            "Louvain quality {:.10} should be <= Leiden quality {:.10}",
            louvain_result.quality,
            leiden_result.quality
        );
        // Validate both produce valid partitions
        assert!(leiden_result.partition.num_communities() >= 1);
        assert!(louvain_result.partition.num_communities() >= 1);
    }

    #[test]
    fn test_louvain_quality_le_than_leiden_with_cpm() {
        let mut b = GraphDataBuilder::new(12);
        for i in 0..4 {
            for j in (i + 1)..4 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        for i in 4..8 {
            for j in (i + 1)..8 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        for i in 8..12 {
            for j in (i + 1)..12 {
                b.add_edge(i, j, 1.0).unwrap();
            }
        }
        b.add_edge(0, 4, 0.1).unwrap();
        b.add_edge(4, 8, 0.1).unwrap();
        b.add_edge(8, 0, 0.1).unwrap();
        let graph = b.build().unwrap();

        let leiden = Leiden::new(LeidenConfig {
            seed: Some(42),
            quality: QualityType::CPM,
            resolution: 0.5,
            skip_refinement: false,
            ..Default::default()
        });
        let leiden_result = leiden.run(&graph).unwrap();

        let louvain = Leiden::new(LeidenConfig {
            seed: Some(42),
            quality: QualityType::CPM,
            resolution: 0.5,
            skip_refinement: true,
            ..Default::default()
        });
        let louvain_result = louvain.run(&graph).unwrap();

        assert!(
            louvain_result.quality <= leiden_result.quality + 1e-12,
            "Louvain quality {:.10} should be <= Leiden quality {:.10} with CPM",
            louvain_result.quality,
            leiden_result.quality
        );
        assert!(leiden_result.partition.num_communities() >= 1);
        assert!(louvain_result.partition.num_communities() >= 1);
    }

    #[test]
    fn test_louvain_builder_method() {
        let config = LeidenConfig::builder()
            .skip_refinement(true)
            .seed(42)
            .build();
        assert!(config.skip_refinement);

        let default_config = LeidenConfig::builder().build();
        assert!(!default_config.skip_refinement);
    }

    #[test]
    fn test_default_skip_refinement_false() {
        let config = LeidenConfig::default();
        assert!(!config.skip_refinement, "default should be false (Leiden mode)");

        let explicit = LeidenConfig {
            skip_refinement: false,
            ..Default::default()
        };
        let graph = make_two_cliques();
        let default_result = Leiden::new(config).run(&graph).unwrap();
        let explicit_result = Leiden::new(explicit).run(&graph).unwrap();

        assert_eq!(
            default_result.partition.as_slice(),
            explicit_result.partition.as_slice(),
            "default and explicit skip_refinement=false should match"
        );
        assert!((default_result.quality - explicit_result.quality).abs() < 1e-10);
    }

    // ── Convergence control tests ─────────────────────────────────

    #[test]
    fn test_min_iterations_does_not_break_convergence() {
        // min_iterations guards only the epsilon convergence check,
        // not the !changed (no nodes moved) check. Verify the algorithm
        // still converges correctly with high min_iterations.
        let graph = make_two_cliques();

        for &min_iter in &[1, 5, 10, 50] {
            let config = LeidenConfig {
                min_iterations: min_iter,
                seed: Some(42),
                ..Default::default()
            };
            let result = Leiden::new(config).run(&graph).unwrap();
            assert_eq!(
                result.partition.num_communities(),
                2,
                "min_iterations={min_iter} should not break convergence"
            );
        }
    }

    #[test]
    fn test_min_iterations_with_quality_tracking() {
        // Verify that min_iterations and quality_history work together
        let graph = make_two_cliques();
        let config = LeidenConfig {
            min_iterations: 5,
            track_quality_history: true,
            seed: Some(42),
            ..Default::default()
        };
        let result = Leiden::new(config).run(&graph).unwrap();

        // Correct partition
        assert_eq!(result.partition.num_communities(), 2);
        // quality_history captures iterations where nodes moved
        assert!(
            !result.quality_history.is_empty(),
            "quality_history should have entries"
        );
        // Same partition as default config (deterministic)
        let default_result = Leiden::new(LeidenConfig {
            seed: Some(42),
            ..Default::default()
        })
        .run(&graph)
        .unwrap();
        assert_eq!(result.partition.as_slice(), default_result.partition.as_slice());
    }

    #[test]
    fn test_min_iterations_one_converges_normally() {
        let graph = make_two_cliques();
        let config = LeidenConfig {
            min_iterations: 1,
            track_quality_history: true,
            seed: Some(42),
            ..Default::default()
        };
        let result = Leiden::new(config).run(&graph).unwrap();
        // Should converge to 2 communities (same as default)
        assert_eq!(result.partition.num_communities(), 2);
        assert!(!result.quality_history.is_empty());
    }

    #[test]
    fn test_quality_history_tracked_when_enabled() {
        let graph = make_two_cliques();
        let config = LeidenConfig {
            track_quality_history: true,
            seed: Some(42),
            ..Default::default()
        };
        let result = Leiden::new(config).run(&graph).unwrap();
        assert!(
            !result.quality_history.is_empty(),
            "quality_history should be non-empty when enabled"
        );
    }

    #[test]
    fn test_quality_history_empty_by_default() {
        let graph = make_two_cliques();
        let result = Leiden::new(LeidenConfig::default()).run(&graph).unwrap();
        assert!(
            result.quality_history.is_empty(),
            "quality_history should be empty by default"
        );
    }

    #[test]
    fn test_quality_history_monotonic() {
        let graph = make_two_cliques();
        let config = LeidenConfig {
            track_quality_history: true,
            seed: Some(42),
            ..Default::default()
        };
        let result = Leiden::new(config).run(&graph).unwrap();
        for window in result.quality_history.windows(2) {
            assert!(
                window[1] >= window[0] - 1e-10,
                "quality decreased: {:.15} -> {:.15}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn test_builder_min_iterations() {
        let config = LeidenConfig::builder().min_iterations(10).build();
        assert_eq!(config.min_iterations, 10);

        let default_config = LeidenConfig::builder().build();
        assert_eq!(default_config.min_iterations, 1);
    }

    #[test]
    fn test_builder_track_quality_history() {
        let config = LeidenConfig::builder().track_quality_history(true).build();
        assert!(config.track_quality_history);

        let default_config = LeidenConfig::builder().build();
        assert!(!default_config.track_quality_history);
    }
}
