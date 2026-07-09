//! Shared algorithm internals for single-layer and multiplex Leiden.

use std::collections::VecDeque;

use crate::leiden::builder::GraphDataBuilder;
use crate::leiden::graph_data::GraphData;
use crate::leiden::partition::Partition;
use crate::leiden::quality::{MoveComponents, QualityFunction};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rustc_hash::FxHashMap;

/// Pre-allocated buffers for the local-moving phase, reused across iterations.
///
/// Eliminates per-iteration heap allocations in `local_moving_generic` by
/// maintaining buffers that grow to the maximum needed size and are cleared
/// (not freed) between calls.
pub(crate) struct LocalMovingBuffers {
    /// Per-layer per-community total out-degree: `[layer][comm]`.
    pub comm_total_degree: Vec<Vec<f64>>,
    /// Per-layer per-community total in-degree: `[layer][comm]`.
    pub comm_in_degree: Vec<Vec<f64>>,
    /// Per-community weighted size: `comm_size[comm]`.
    pub comm_size: Vec<f64>,
    /// Per-layer per-community out-edge weight from the current node.
    pub layer_out_weights: Vec<Vec<f64>>,
    /// Per-layer per-community in-edge weight from the current node.
    pub layer_in_weights: Vec<Vec<f64>>,
    /// Communities whose weight buffers are non-zero and need clearing.
    pub touched_list: Vec<usize>,
    /// Mark array for touched communities.
    pub touched_mark: Vec<bool>,
    /// Whether a node is currently in the work queue.
    pub in_queue: Vec<bool>,
    /// Per-layer `2 * total_weight` values.
    pub two_m_values: Vec<f64>,
}

impl LocalMovingBuffers {
    /// Create buffers pre-allocated for `capacity` nodes and `num_layers` layers.
    pub fn new(capacity: usize, num_layers: usize) -> Self {
        Self {
            comm_total_degree: vec![vec![0.0; capacity]; num_layers],
            comm_in_degree: vec![vec![0.0; capacity]; num_layers],
            comm_size: vec![0.0; capacity],
            layer_out_weights: vec![vec![0.0; capacity]; num_layers],
            layer_in_weights: vec![vec![0.0; capacity]; num_layers],
            touched_list: Vec::with_capacity(64),
            touched_mark: vec![false; capacity],
            in_queue: vec![false; capacity],
            two_m_values: vec![0.0; num_layers],
        }
    }

    /// Ensure buffers can hold at least `n` communities and `num_layers` layers,
    /// then clear them for reuse. Does not shrink.
    pub fn resize_and_clear(&mut self, n: usize, num_layers: usize) {
        if num_layers > self.comm_total_degree.len() {
            self.comm_total_degree
                .resize_with(num_layers, || vec![0.0; n]);
            self.comm_in_degree.resize_with(num_layers, || vec![0.0; n]);
        }
        for arr in &mut self.comm_total_degree[..num_layers] {
            if n > arr.len() {
                arr.resize(n, 0.0);
            }
            arr[..n].fill(0.0);
        }
        for arr in &mut self.comm_in_degree[..num_layers] {
            if n > arr.len() {
                arr.resize(n, 0.0);
            }
            arr[..n].fill(0.0);
        }
        if n > self.comm_size.len() {
            self.comm_size.resize(n, 0.0);
            self.touched_mark.resize(n, false);
            self.in_queue.resize(n, false);
        }
        self.comm_size[..n].fill(0.0);
        self.touched_mark[..n].fill(false);
        self.in_queue[..n].fill(false);

        if num_layers > self.layer_out_weights.len() {
            self.layer_out_weights
                .resize_with(num_layers, || vec![0.0; n]);
            self.layer_in_weights
                .resize_with(num_layers, || vec![0.0; n]);
        }
        for lw in &mut self.layer_out_weights[..num_layers] {
            if n > lw.len() {
                lw.resize(n, 0.0);
            }
            lw[..n].fill(0.0);
        }
        for lw in &mut self.layer_in_weights[..num_layers] {
            if n > lw.len() {
                lw.resize(n, 0.0);
            }
            lw[..n].fill(0.0);
        }

        if num_layers > self.two_m_values.len() {
            self.two_m_values.resize(num_layers, 0.0);
        }
        self.two_m_values[..num_layers].fill(0.0);

        for &idx in &self.touched_list {
            if idx < self.touched_mark.len() {
                self.touched_mark[idx] = false;
            }
        }
        self.touched_list.clear();
    }
}

/// Pre-allocated buffers for refinement phase, reused across communities.
///
/// Eliminates repeated allocations in `refine_community_generic` by maintaining
/// buffers that can be resized and cleared between community refinements.
pub(crate) struct RefinementBuffers {
    comm_total_degree: Vec<Vec<f64>>,
    comm_in_degree: Vec<Vec<f64>>,
    comm_size: Vec<f64>,
    layer_out_weights: Vec<Vec<f64>>,
    layer_in_weights: Vec<Vec<f64>>,
    touched_list: Vec<usize>,
    touched_mark: Vec<bool>,
    /// Per-layer `2 * total_weight` values.
    two_m_values: Vec<f64>,
}

impl RefinementBuffers {
    pub fn new(capacity: usize, num_layers: usize) -> Self {
        Self {
            comm_total_degree: vec![vec![0.0; capacity]; num_layers],
            comm_in_degree: vec![vec![0.0; capacity]; num_layers],
            comm_size: vec![0.0; capacity],
            layer_out_weights: vec![vec![0.0; capacity]; num_layers],
            layer_in_weights: vec![vec![0.0; capacity]; num_layers],
            touched_list: Vec::with_capacity(64),
            touched_mark: vec![false; capacity],
            two_m_values: vec![0.0; num_layers],
        }
    }

    /// Ensure buffers can hold at least `new_capacity` entries, then clear
    /// them for reuse. Also populates `two_m_values` from `layers`.
    pub fn resize_and_clear(&mut self, new_capacity: usize, layers: &[GraphData]) {
        let num_layers = layers.len();

        if num_layers > self.comm_total_degree.len() {
            self.comm_total_degree
                .resize_with(num_layers, || vec![0.0; new_capacity]);
            self.comm_in_degree
                .resize_with(num_layers, || vec![0.0; new_capacity]);
        }
        for arr in &mut self.comm_total_degree[..num_layers] {
            if new_capacity > arr.len() {
                arr.resize(new_capacity, 0.0);
            }
            arr[..new_capacity].fill(0.0);
        }
        for arr in &mut self.comm_in_degree[..num_layers] {
            if new_capacity > arr.len() {
                arr.resize(new_capacity, 0.0);
            }
            arr[..new_capacity].fill(0.0);
        }

        if new_capacity > self.comm_size.len() {
            self.comm_size.resize(new_capacity, 0.0);
            for layer_weights in &mut self.layer_out_weights {
                layer_weights.resize(new_capacity, 0.0);
            }
            for layer_weights in &mut self.layer_in_weights {
                layer_weights.resize(new_capacity, 0.0);
            }
            self.touched_mark.resize(new_capacity, false);
        }
        self.comm_size[..new_capacity].fill(0.0);
        for layer_weights in &mut self.layer_out_weights[..num_layers] {
            if new_capacity > layer_weights.len() {
                layer_weights.resize(new_capacity, 0.0);
            }
            layer_weights[..new_capacity].fill(0.0);
        }
        for layer_weights in &mut self.layer_in_weights[..num_layers] {
            if new_capacity > layer_weights.len() {
                layer_weights.resize(new_capacity, 0.0);
            }
            layer_weights[..new_capacity].fill(0.0);
        }
        for &idx in &self.touched_list {
            self.touched_mark[idx] = false;
        }
        self.touched_list.clear();

        if num_layers > self.two_m_values.len() {
            self.two_m_values.resize(num_layers, 0.0);
        }
        for (l, layer) in layers.iter().enumerate() {
            self.two_m_values[l] = 2.0 * layer.total_weight();
        }
    }
}

/// Find the best community to move a node to.
///
/// `delta_fn(target_comm) -> f64` returns the quality delta for moving to that community.
/// Returns `(best_community, best_delta)`. Stays at current if no improvement > epsilon.
#[inline]
pub(crate) fn find_best_community(
    candidates: impl Iterator<Item = usize>,
    current_community: usize,
    epsilon: f64,
    max_comm_size: usize,
    comm_size: &[f64],
    node_weight: f64,
    delta_fn: impl Fn(usize) -> f64,
) -> (usize, f64) {
    let mut best_community = current_community;
    let mut best_delta = epsilon;
    for target_comm in candidates {
        if max_comm_size > 0 && comm_size[target_comm] + node_weight > max_comm_size as f64 {
            continue;
        }
        let delta = delta_fn(target_comm);
        if delta > best_delta {
            best_delta = delta;
            best_community = target_comm;
        }
    }
    (best_community, best_delta)
}

/// Target for a community move: either a partition or a refined map.
pub(crate) enum MoveTarget<'a> {
    /// Local moving phase: update the partition directly.
    Partition(&'a mut Partition),
    /// Refinement phase: update the refined map array.
    RefinedMap(&'a mut [usize]),
}

/// Per-node contribution values used during a community move.
pub(crate) struct NodeContribution<'a> {
    /// Per-layer out-degree of the moved node.
    pub k_v_out: &'a [f64],
    /// Per-layer in-degree of the moved node.
    pub k_v_in: &'a [f64],
    /// Node weight (same across layers).
    pub weight: f64,
}

/// Mutable community statistics slices updated during moves.
///
/// Degree arrays are indexed as `[layer][community]` to keep per-layer
/// sigma values separate. `size` is a flat `[community]` slice because
/// node weights are shared across layers.
pub(crate) struct CommunityStats<'a> {
    pub total_degree_out: &'a mut [Vec<f64>],
    /// Per-layer per-community total in-degree sigma: `[layer][comm]`.
    pub sigma_in: &'a mut [Vec<f64>],
    pub size: &'a mut [f64],
}

/// Configuration parameters shared across local-moving and refinement.
pub(crate) struct MovingConfig {
    pub max_comm_size: usize,
    pub epsilon: f64,
}

/// A community subset being refined (community ID + its member nodes).
pub(crate) struct CommunitySubset<'a> {
    pub community: usize,
    pub nodes: &'a [usize],
}

/// Apply a community move and update statistics arrays.
///
/// Loops over layers to update per-layer degree statistics.
/// Size update is layer-independent (shared node set).
#[inline]
pub(crate) fn apply_move(
    target: MoveTarget<'_>,
    node: usize,
    old_comm: usize,
    new_comm: usize,
    contribution: NodeContribution<'_>,
    stats: &mut CommunityStats<'_>,
) {
    match target {
        MoveTarget::RefinedMap(map) => map[node] = new_comm,
        MoveTarget::Partition(partition) => partition.move_node(node, new_comm),
    }
    for l in 0..contribution.k_v_out.len() {
        stats.total_degree_out[l][old_comm] -= contribution.k_v_out[l];
        stats.total_degree_out[l][new_comm] += contribution.k_v_out[l];
        if !stats.sigma_in[l].is_empty() {
            stats.sigma_in[l][old_comm] -= contribution.k_v_in[l];
            stats.sigma_in[l][new_comm] += contribution.k_v_in[l];
        }
    }
    stats.size[old_comm] -= contribution.weight;
    stats.size[new_comm] += contribution.weight;
}

/// Initialize community statistics into per-layer arrays.
///
/// `community_of_fn` maps node -> community index.
/// Degree arrays are indexed `[layer][community]`; `comm_size_out` is flat
/// `[community]` (node weights are shared across layers, counted once).
pub(crate) fn init_community_stats_into(
    layers: &[GraphData],
    community_of_fn: impl Fn(usize) -> usize,
    comm_total_degree_out: &mut [Vec<f64>],
    comm_sigma_in: &mut [Vec<f64>],
    comm_size_out: &mut [f64],
) {
    let n = comm_size_out.len();
    for node in 0..n {
        let comm = community_of_fn(node);
        comm_size_out[comm] += layers[0].node_weight(node);
    }
    for (l, layer) in layers.iter().enumerate() {
        for node in 0..n {
            let comm = community_of_fn(node);
            comm_total_degree_out[l][comm] += layer.out_degree_of(node);
            comm_sigma_in[l][comm] += layer.in_degree_of(node);
        }
    }
}

/// Accumulate one node's edges into the aggregated edge map.
///
/// For directed graphs: maps (u→v) through orig_to_agg.
/// For undirected graphs: canonicalizes keys to (min, max) to avoid double-counting;
/// self-loops use (u, u); skips edges where v <= u (each undirected edge stored twice in CSR).
#[inline]
pub(crate) fn aggregate_node_edges_into(
    source: &GraphData,
    u: usize,
    orig_to_agg: &[usize],
    directed: bool,
    map: &mut FxHashMap<(usize, usize), f64>,
) {
    let ru = orig_to_agg[u];
    if directed {
        for (v, w) in source.neighbors(u) {
            let rv = orig_to_agg[v];
            *map.entry((ru, rv)).or_default() += w;
        }
    } else {
        for (v, w) in source.neighbors(u) {
            if u == v {
                *map.entry((ru, ru)).or_default() += w;
            } else if v > u {
                let rv = orig_to_agg[v];
                let key = if ru <= rv { (ru, rv) } else { (rv, ru) };
                *map.entry(key).or_default() += w;
            }
        }
    }
}

/// Build the orig_to_agg mapping from a refined partition.
///
/// Returns `(orig_to_agg, agg_n)` where `orig_to_agg[original_node] = aggregate_node_id`
/// and `agg_n` is the number of aggregate nodes.
pub(crate) fn build_orig_to_agg_mapping(
    n: usize,
    refined_partition: &Partition,
) -> (Vec<usize>, usize) {
    let mut orig_to_agg: Vec<usize> = vec![0; n];
    let mut comm_to_agg: FxHashMap<usize, usize> = FxHashMap::default();
    let mut next_id = 0usize;
    for (node, entry) in orig_to_agg.iter_mut().enumerate() {
        let c = refined_partition.community_of(node);
        let agg_id = *comm_to_agg.entry(c).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        *entry = agg_id;
    }
    (orig_to_agg, next_id)
}

/// Build the aggregated graph from collected edges and node weights.
///
/// Takes ownership of `orig_to_agg` and returns `(GraphData, Vec<usize>, Partition)`.
/// The `node_weight_fn` closure provides the weight for each original node.
pub(crate) fn build_aggregated_graph(
    orig_to_agg: Vec<usize>,
    agg_n: usize,
    directed: bool,
    agg_edges_map: FxHashMap<(usize, usize), f64>,
    coarse_partition: &Partition,
    node_weight_fn: impl Fn(usize) -> f64,
) -> crate::leiden::error::Result<(crate::leiden::graph_data::GraphData, Vec<usize>, Partition)> {
    let mut agg_edges: Vec<((usize, usize), f64)> = agg_edges_map.into_iter().collect();
    agg_edges.sort_by_key(|&((u, v), _)| (u, v));

    let mut agg_node_weight: Vec<f64> = vec![0.0; agg_n];
    for (orig, &agg_node) in orig_to_agg.iter().enumerate() {
        agg_node_weight[agg_node] += node_weight_fn(orig);
    }

    let mut builder = GraphDataBuilder::new(agg_n);
    if directed {
        builder = builder.directed();
    }
    for &((u, v), w) in &agg_edges {
        builder.add_edge(u, v, w)?;
    }
    for (node, &nw) in agg_node_weight.iter().enumerate() {
        if nw != 1.0 {
            builder.set_node_weight(node, nw)?;
        }
    }
    let agg_data = builder.build()?;

    let mut agg_initial = Partition::new(agg_n);
    for (orig, &agg_node) in orig_to_agg.iter().enumerate() {
        let coarse_comm = coarse_partition.community_of(orig);
        agg_initial.move_node(agg_node, coarse_comm);
    }
    agg_initial.renumber();

    Ok((agg_data, orig_to_agg, agg_initial))
}

/// Generic refinement wrapper: collect community nodes, shuffle, dispatch refinement
/// per community (sequential or parallel), and apply moves.
///
/// The `refine_fn` closure receives `(community_index, community_node_list, &mut buffers)` and
/// returns a list of `(node, new_community)` moves.
pub(crate) fn refinement_generic(
    n: usize,
    _num_layers: usize,
    partition: &Partition,
    rng: &mut StdRng,
    buffers: &mut RefinementBuffers,
    refine_fn: impl Fn(usize, &[usize], &mut RefinementBuffers) -> Vec<(usize, usize)> + Send + Sync,
) -> Partition {
    let mut refined = Partition::new(n);

    let num_comms = partition.num_communities();
    let mut community_nodes: Vec<Vec<usize>> = vec![Vec::new(); num_comms];
    for node in 0..n {
        community_nodes[partition.community_of(node)].push(node);
    }
    for nodes in &mut community_nodes {
        nodes.shuffle(rng);
    }

    let results: Vec<Vec<(usize, usize)>> = {
        #[cfg(feature = "rayon")]
        {
            let par_threshold = std::cmp::max(4, rayon::current_num_threads() * 2);
            if num_comms > par_threshold {
                use rayon::prelude::*;
                let num_layers = buffers.comm_total_degree.len();
                community_nodes
                    .par_iter()
                    .enumerate()
                    .map_init(
                        || RefinementBuffers::new(n, num_layers),
                        |thread_buffers, (community, nodes)| {
                            refine_fn(community, nodes, thread_buffers)
                        },
                    )
                    .collect()
            } else {
                community_nodes
                    .iter()
                    .enumerate()
                    .map(|(community, nodes)| refine_fn(community, nodes, buffers))
                    .collect()
            }
        }
        #[cfg(not(feature = "rayon"))]
        {
            community_nodes
                .iter()
                .enumerate()
                .map(|(community, nodes)| refine_fn(community, nodes, buffers))
                .collect()
        }
    };

    for moves in &results {
        for &(node, new_comm) in moves {
            refined.move_node(node, new_comm);
        }
    }

    refined.renumber();
    refined
}

/// Unified local moving for single-layer and multiplex Leiden.
///
/// Single-layer: `layers = &[data], layer_weights = &[1.0]`
/// Multi-layer: pass all layers and their weights.
///
/// Uses per-layer pre-computed neighbor weight buffers instead of re-scanning,
/// accumulating out-edge and in-edge weights separately for each layer in one pass.
///
/// `buffers` are reused across calls to avoid repeated allocations.
pub(crate) fn local_moving_generic(
    layers: &[GraphData],
    layer_weights: &[f64],
    partition: &mut Partition,
    quality: &(dyn QualityFunction + Sync),
    rng: &mut StdRng,
    cfg: &MovingConfig,
    buffers: &mut LocalMovingBuffers,
) -> bool {
    let n = layers[0].node_count();
    if n == 0 {
        return false;
    }

    let num_layers = layers.len();

    let total_weight: f64 = layers.iter().map(|l| l.total_weight()).sum();
    if total_weight <= 0.0 {
        return false;
    }

    buffers.resize_and_clear(n, num_layers);

    for (l, layer) in layers.iter().enumerate() {
        buffers.two_m_values[l] = 2.0 * layer.total_weight();
    }
    let total_node_weight: f64 = layers[0].total_node_weight();

    init_community_stats_into(
        layers,
        |node| partition.community_of(node),
        &mut buffers.comm_total_degree,
        &mut buffers.comm_in_degree,
        &mut buffers.comm_size[..n],
    );
    let comm_total_degree = &mut buffers.comm_total_degree;
    let comm_in_degree = &mut buffers.comm_in_degree;
    let comm_size = &mut buffers.comm_size;

    let mut order: Vec<usize> = (0..n)
        .filter(|&node| layers.iter().any(|l| l.degree_of(node) > 0.0))
        .collect();
    order.shuffle(rng);
    let mut queue: VecDeque<usize> = order.into_iter().collect();
    let in_queue = &mut buffers.in_queue;
    for &node in &queue {
        in_queue[node] = true;
    }

    let layer_out_weights = &mut buffers.layer_out_weights;
    let layer_in_weights = &mut buffers.layer_in_weights;
    let two_m_values = &buffers.two_m_values;

    let mut changed = false;
    let touched_list = &mut buffers.touched_list;
    let touched_mark = &mut buffers.touched_mark;

    // Per-layer degree buffers for apply_move
    let mut k_v_out_buf: Vec<f64> = vec![0.0; num_layers];
    let mut k_v_in_buf: Vec<f64> = vec![0.0; num_layers];

    while let Some(node) = queue.pop_front() {
        in_queue[node] = false;
        let current_community = partition.community_of(node);

        for l in 0..num_layers {
            layer_out_weights[l][current_community] = 0.0;
            layer_in_weights[l][current_community] = 0.0;
        }

        let mut current_touched = false;

        for (l, layer) in layers.iter().enumerate() {
            let (targets, weights) = layer.neighbor_slices(node);
            for i in 0..targets.len() {
                let neighbor = targets[i];
                let weight = weights[i];
                if neighbor == node {
                    continue;
                }
                let comm = partition.community_of(neighbor);
                if layer_out_weights[l][comm] == 0.0 && layer_in_weights[l][comm] == 0.0 {
                    if comm == current_community {
                        current_touched = true;
                    } else if !touched_mark[comm] {
                        touched_mark[comm] = true;
                        touched_list.push(comm);
                    }
                }
                layer_out_weights[l][comm] += weight;
            }
            let (in_targets, in_weights) = layer.in_neighbor_slices(node);
            for i in 0..in_targets.len() {
                let neighbor = in_targets[i];
                let weight = in_weights[i];
                if neighbor == node {
                    continue;
                }
                let comm = partition.community_of(neighbor);
                if layer_out_weights[l][comm] == 0.0 && layer_in_weights[l][comm] == 0.0 {
                    if comm == current_community {
                        current_touched = true;
                    } else if !touched_mark[comm] {
                        touched_mark[comm] = true;
                        touched_list.push(comm);
                    }
                }
                layer_in_weights[l][comm] += weight;
            }
        }

        for (l, layer) in layers.iter().enumerate() {
            k_v_out_buf[l] = layer.out_degree_of(node);
            k_v_in_buf[l] = layer.in_degree_of(node);
        }
        let node_weight = layers[0].node_weight(node);

        let (best_community, _) = find_best_community(
            touched_list.iter().copied(),
            current_community,
            cfg.epsilon,
            cfg.max_comm_size,
            &comm_size[..n],
            node_weight,
            |target_comm| {
                let mut total_delta = 0.0f64;
                for (l, layer) in layers.iter().enumerate() {
                    let delta = quality.delta_move_from_components(&MoveComponents {
                        two_m: two_m_values[l],
                        node_weight,
                        total_node_weight,
                        k_v_out: k_v_out_buf[l],
                        k_v_to_target_out: layer_out_weights[l][target_comm],
                        k_v_to_current_out: layer_out_weights[l][current_community],
                        sigma_tot_target_out: comm_total_degree[l][target_comm],
                        sigma_tot_current_out: comm_total_degree[l][current_community],
                        k_v_in: k_v_in_buf[l],
                        k_v_to_target_in: layer_in_weights[l][target_comm],
                        k_v_to_current_in: layer_in_weights[l][current_community],
                        sigma_tot_target_in: comm_in_degree[l][target_comm],
                        sigma_tot_current_in: comm_in_degree[l][current_community],
                        n_target: comm_size[target_comm],
                        n_current: comm_size[current_community],
                        directed: layer.is_directed(),
                    });
                    total_delta += layer_weights[l] * delta;
                }
                total_delta
            },
        );

        for l in 0..num_layers {
            // current_community was not added to touched_list (to avoid duplicate tracking),
            // so clear it separately only when it actually received edge weight.
            if current_touched {
                layer_out_weights[l][current_community] = 0.0;
                layer_in_weights[l][current_community] = 0.0;
            }
            for &comm in &*touched_list {
                layer_out_weights[l][comm] = 0.0;
                layer_in_weights[l][comm] = 0.0;
            }
        }
        for &idx in &*touched_list {
            touched_mark[idx] = false;
        }
        touched_list.clear();

        if best_community != current_community {
            apply_move(
                MoveTarget::Partition(&mut *partition),
                node,
                current_community,
                best_community,
                NodeContribution {
                    k_v_out: &k_v_out_buf,
                    k_v_in: &k_v_in_buf,
                    weight: node_weight,
                },
                &mut CommunityStats {
                    total_degree_out: comm_total_degree,
                    sigma_in: comm_in_degree,
                    size: &mut comm_size[..n],
                },
            );
            changed = true;

            for layer in layers {
                let (targets, _) = layer.neighbor_slices(node);
                for &neighbor in targets {
                    if !in_queue[neighbor] {
                        queue.push_back(neighbor);
                        in_queue[neighbor] = true;
                    }
                }
                let (in_targets, _) = layer.in_neighbor_slices(node);
                for &neighbor in in_targets {
                    if !in_queue[neighbor] {
                        queue.push_back(neighbor);
                        in_queue[neighbor] = true;
                    }
                }
            }
        }
    }

    changed
}

/// Unified refinement for single-layer and multiplex Leiden.
///
/// Refines communities by splitting them based on quality improvement.
/// Only considers neighbors within the same coarse community.
/// Uses per-layer pre-computed neighbor weights for delta computation.
///
/// Single-layer: `layers = &[data], layer_weights = &[1.0]`
/// Multi-layer: pass all layers and their weights.
///
/// `buffers` are reused across calls to avoid repeated allocations.
pub(crate) fn refine_community_generic(
    layers: &[GraphData],
    layer_weights: &[f64],
    partition: &Partition,
    quality: &(dyn QualityFunction + Sync),
    subset: &CommunitySubset,
    cfg: &MovingConfig,
    buffers: &mut RefinementBuffers,
) -> Vec<(usize, usize)> {
    if subset.nodes.len() <= 1 {
        return Vec::new();
    }

    let num_layers = layers.len();
    let total_node_weight: f64 = layers[0].total_node_weight();

    let max_node_id = subset.nodes.iter().copied().max().unwrap_or(0);
    let mut refined_map: Vec<usize> = (0..=max_node_id).collect();

    buffers.resize_and_clear(max_node_id + 1, layers);

    // Split borrow: separate fields so the closure can read two_m_values
    // while other fields are accessed mutably through reborrow.
    let two_m_values = &buffers.two_m_values;
    let comm_total_degree = &mut buffers.comm_total_degree;
    let comm_in_degree = &mut buffers.comm_in_degree;
    let comm_size = &mut buffers.comm_size;
    let layer_out_weights = &mut buffers.layer_out_weights;
    let layer_in_weights = &mut buffers.layer_in_weights;
    let touched_list = &mut buffers.touched_list;
    let touched_mark = &mut buffers.touched_mark;

    for &node in subset.nodes {
        for (l, layer) in layers.iter().enumerate() {
            comm_total_degree[l][node] += layer.out_degree_of(node);
            comm_in_degree[l][node] += layer.in_degree_of(node);
        }
        comm_size[node] += layers[0].node_weight(node);
    }

    let mut k_v_out_buf: Vec<f64> = vec![0.0; num_layers];
    let mut k_v_in_buf: Vec<f64> = vec![0.0; num_layers];

    for &node in subset.nodes {
        let current_refined = refined_map[node];

        for l in 0..num_layers {
            layer_out_weights[l][current_refined] = 0.0;
            layer_in_weights[l][current_refined] = 0.0;
        }

        for (l, layer) in layers.iter().enumerate() {
            let (targets, weights) = layer.neighbor_slices(node);
            for i in 0..targets.len() {
                let neighbor = targets[i];
                let weight = weights[i];
                if partition.community_of(neighbor) != subset.community {
                    continue;
                }
                if neighbor == node {
                    continue;
                }
                let rc = refined_map[neighbor];
                if layer_out_weights[l][rc] == 0.0
                    && layer_in_weights[l][rc] == 0.0
                    && rc != current_refined
                    && !touched_mark[rc]
                {
                    touched_mark[rc] = true;
                    touched_list.push(rc);
                }
                layer_out_weights[l][rc] += weight;
            }
            let (in_targets, in_weights) = layer.in_neighbor_slices(node);
            for i in 0..in_targets.len() {
                let neighbor = in_targets[i];
                let weight = in_weights[i];
                if partition.community_of(neighbor) != subset.community {
                    continue;
                }
                if neighbor == node {
                    continue;
                }
                let rc = refined_map[neighbor];
                if layer_out_weights[l][rc] == 0.0
                    && layer_in_weights[l][rc] == 0.0
                    && rc != current_refined
                    && !touched_mark[rc]
                {
                    touched_mark[rc] = true;
                    touched_list.push(rc);
                }
                layer_in_weights[l][rc] += weight;
            }
        }

        for (l, layer) in layers.iter().enumerate() {
            k_v_out_buf[l] = layer.out_degree_of(node);
            k_v_in_buf[l] = layer.in_degree_of(node);
        }
        let node_weight = layers[0].node_weight(node);

        let (best_refined, _) = find_best_community(
            touched_list.iter().copied(),
            current_refined,
            cfg.epsilon,
            0,
            &comm_size[..],
            node_weight,
            |target_comm| {
                let mut total_delta = 0.0f64;
                for (l, layer) in layers.iter().enumerate() {
                    let delta = quality.delta_move_from_components(&MoveComponents {
                        two_m: two_m_values[l],
                        node_weight,
                        total_node_weight,
                        k_v_out: k_v_out_buf[l],
                        k_v_to_target_out: layer_out_weights[l][target_comm],
                        k_v_to_current_out: layer_out_weights[l][current_refined],
                        sigma_tot_target_out: comm_total_degree[l][target_comm],
                        sigma_tot_current_out: comm_total_degree[l][current_refined],
                        k_v_in: k_v_in_buf[l],
                        k_v_to_target_in: layer_in_weights[l][target_comm],
                        k_v_to_current_in: layer_in_weights[l][current_refined],
                        sigma_tot_target_in: comm_in_degree[l][target_comm],
                        sigma_tot_current_in: comm_in_degree[l][current_refined],
                        n_target: comm_size[target_comm],
                        n_current: comm_size[current_refined],
                        directed: layer.is_directed(),
                    });
                    total_delta += layer_weights[l] * delta;
                }
                total_delta
            },
        );

        for l in 0..num_layers {
            for &rc in &*touched_list {
                layer_out_weights[l][rc] = 0.0;
                layer_in_weights[l][rc] = 0.0;
            }
            layer_out_weights[l][current_refined] = 0.0;
            layer_in_weights[l][current_refined] = 0.0;
        }
        for &idx in &*touched_list {
            touched_mark[idx] = false;
        }
        touched_list.clear();

        if best_refined != current_refined {
            apply_move(
                MoveTarget::RefinedMap(&mut refined_map),
                node,
                current_refined,
                best_refined,
                NodeContribution {
                    k_v_out: &k_v_out_buf,
                    k_v_in: &k_v_in_buf,
                    weight: node_weight,
                },
                &mut CommunityStats {
                    total_degree_out: comm_total_degree,
                    sigma_in: comm_in_degree,
                    size: &mut comm_size[..],
                },
            );
        }
    }

    subset
        .nodes
        .iter()
        .filter_map(|&node| {
            let rc = refined_map[node];
            if rc != node {
                Some((node, rc))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "algorithm_tests.rs"]
mod per_layer_tests;
