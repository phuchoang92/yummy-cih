use super::*;
use crate::leiden_impl::builder::GraphDataBuilder;

/// RED TEST: Verify that init_community_stats_into populates PER-LAYER arrays,
/// NOT accumulated across layers.
///
/// With the current buggy code, init_community_stats_into accumulates
/// comm_total_degree across ALL layers into a single flat array.
/// This test creates 2 layers with different degree structures and verifies
/// that per-layer sigma values are stored separately.
#[test]
fn test_init_community_stats_per_layer_not_accumulated() {
    // Layer 1: triangle 0-1-2, node 3 isolated
    //   degrees: node 0=2, node 1=2, node 2=2, node 3=0
    let mut b1 = GraphDataBuilder::new(4);
    b1.add_edge(0, 1, 1.0).unwrap();
    b1.add_edge(1, 2, 1.0).unwrap();
    b1.add_edge(0, 2, 1.0).unwrap();
    let layer1 = b1.build().unwrap();

    // Layer 2: chain 0-1-2-3 with weight 2.0
    //   degrees: node 0=2, node 1=4, node 2=4, node 3=2
    let mut b2 = GraphDataBuilder::new(4);
    b2.add_edge(0, 1, 2.0).unwrap();
    b2.add_edge(1, 2, 2.0).unwrap();
    b2.add_edge(2, 3, 2.0).unwrap();
    let layer2 = b2.build().unwrap();

    let layers = [layer1, layer2];
    let n = 4;

    // All nodes in community 0 (trivial partition)
    let community_of = |_node: usize| -> usize { 0 };

    // Per-layer arrays
    let mut comm_total_degree: Vec<Vec<f64>> = vec![vec![0.0; n]; 2];
    let mut comm_in_degree: Vec<Vec<f64>> = vec![vec![0.0; n]; 2];
    let mut comm_size: Vec<f64> = vec![0.0; n];

    init_community_stats_into(
        &layers,
        community_of,
        &mut comm_total_degree,
        &mut comm_in_degree,
        &mut comm_size,
    );

    // Community 0 contains all 4 nodes.
    // Layer 1 total degree for comm 0 should be: 2+2+2+0 = 6 (NOT 6+12=18)
    assert!(
        (comm_total_degree[0][0] - 6.0).abs() < 1e-10,
        "Layer 0 comm_total_degree should be 6.0 (triangle degrees), got {}",
        comm_total_degree[0][0],
    );

    // Layer 2 total degree for comm 0 should be: 2+4+4+2 = 12
    assert!(
        (comm_total_degree[1][0] - 12.0).abs() < 1e-10,
        "Layer 1 comm_total_degree should be 12.0 (chain degrees), got {}",
        comm_total_degree[1][0],
    );

    // comm_size should NOT be accumulated across layers (same node set).
    // Each node has weight 1.0, 4 nodes → comm_size[0] = 4.0
    assert!(
        (comm_size[0] - 4.0).abs() < 1e-10,
        "comm_size should be 4.0 (counted once, not accumulated), got {}",
        comm_size[0],
    );
}

// ── find_best_community tests ──

/// Verify that find_best_community picks the candidate with the highest positive delta.
///
/// Setup: 3-node chain 0-1-2, node 1 starts in its own community (1).
/// Candidate communities 0 and 2 have deltas of 0.1 and 0.3 respectively.
/// The function should pick community 2.
#[test]
fn test_find_best_community_picks_highest_delta() {
    let comm_size: Vec<f64> = vec![1.0, 1.0, 1.0];
    let node_weight = 1.0;
    let candidates = [0usize, 2].into_iter();
    let delta_fn = |target: usize| -> f64 {
        match target {
            0 => 0.1,
            2 => 0.3,
            _ => 0.0,
        }
    };
    let (best, delta) = find_best_community(
        candidates,
        1, // current community
        1e-10,
        0, // no max size
        &comm_size,
        node_weight,
        delta_fn,
    );
    assert_eq!(best, 2, "should pick community with highest delta");
    assert!((delta - 0.3).abs() < 1e-10, "delta should be 0.3");
}

/// Verify that find_best_community stays at current community when no delta exceeds epsilon.
#[test]
fn test_find_best_community_stays_when_no_improvement() {
    let comm_size: Vec<f64> = vec![1.0, 1.0, 1.0];
    let candidates = [0usize, 2].into_iter();
    let delta_fn = |_target: usize| -> f64 { 1e-20 };
    let (best, _) = find_best_community(candidates, 1, 1e-10, 0, &comm_size, 1.0, delta_fn);
    assert_eq!(
        best, 1,
        "should stay at current community when all deltas < epsilon"
    );
}

/// Verify that max_comm_size rejects candidates that would exceed the size limit.
#[test]
fn test_find_best_community_respects_max_comm_size() {
    // community 0 already has size 5, max is 5.5 — adding node_weight=1 exceeds it.
    let comm_size: Vec<f64> = vec![5.0, 1.0, 2.0];
    let candidates = [0usize, 2].into_iter();
    let delta_fn = |target: usize| -> f64 {
        // community 0 would be great but is too big
        if target == 0 {
            100.0
        } else {
            0.5
        }
    };
    let (best, delta) = find_best_community(
        candidates, 1, 1e-10, 5, // max_comm_size as usize (5 nodes)
        &comm_size, 1.0, // node_weight
        delta_fn,
    );
    // community 0 is skipped (5+1 > 5), community 2 is chosen
    assert_eq!(best, 2, "should skip community that exceeds max size");
    assert!((delta - 0.5).abs() < 1e-10);
}

// ── apply_move tests ──

/// Verify that apply_move updates partition and community statistics correctly.
///
/// Setup: 3-node chain, node 1 moves from community 1 to community 0.
/// After the move, partition should reflect the new community, and stats
/// (total_degree, in_degree, size) should be updated.
#[test]
fn test_apply_move_updates_partition_and_stats() {
    let mut partition = Partition::new(3);
    // All nodes start in their own communities: 0→0, 1→1, 2→2
    assert_eq!(partition.community_of(1), 1);

    let mut total_degree: Vec<Vec<f64>> = vec![vec![2.0, 2.0, 2.0]];
    let mut in_degree: Vec<Vec<f64>> = vec![vec![2.0, 2.0, 2.0]];
    let mut size: Vec<f64> = vec![1.0, 1.0, 1.0];

    apply_move(
        MoveTarget::Partition(&mut partition),
        1, // node
        1, // old community
        0, // new community
        NodeContribution {
            k_v_out: &[2.0],
            k_v_in: &[2.0],
            weight: 1.0,
        },
        &mut CommunityStats {
            total_degree_out: &mut total_degree,
            sigma_in: &mut in_degree,
            size: &mut size,
        },
    );

    // Partition should now have node 1 in community 0
    assert_eq!(partition.community_of(1), 0);
    // old community 1 lost degree/size; new community 0 gained them
    assert!(
        (total_degree[0][0] - 4.0).abs() < 1e-10,
        "comm 0 total_degree should be 4.0"
    );
    assert!(
        (total_degree[0][1] - 0.0).abs() < 1e-10,
        "comm 1 total_degree should be 0.0"
    );
    assert!(
        (in_degree[0][0] - 4.0).abs() < 1e-10,
        "comm 0 in_degree should be 4.0"
    );
    assert!(
        (in_degree[0][1] - 0.0).abs() < 1e-10,
        "comm 1 in_degree should be 0.0"
    );
    assert!((size[0] - 2.0).abs() < 1e-10, "comm 0 size should be 2.0");
    assert!((size[1] - 0.0).abs() < 1e-10, "comm 1 size should be 0.0");
}

/// Verify that apply_move with RefineMap target updates the map array.
#[test]
fn test_apply_move_refined_map() {
    let mut refined_map: Vec<usize> = vec![0, 1, 2];

    let mut total_degree: Vec<Vec<f64>> = vec![vec![2.0, 2.0, 2.0]];
    let mut in_degree: Vec<Vec<f64>> = vec![vec![]]; // empty → skipped
    let mut size: Vec<f64> = vec![1.0, 1.0, 1.0];

    apply_move(
        MoveTarget::RefinedMap(&mut refined_map),
        2, // node
        2, // old community
        0, // new community
        NodeContribution {
            k_v_out: &[2.0],
            k_v_in: &[0.0],
            weight: 1.0,
        },
        &mut CommunityStats {
            total_degree_out: &mut total_degree,
            sigma_in: &mut in_degree,
            size: &mut size,
        },
    );

    assert_eq!(
        refined_map[2], 0,
        "refined map should update node 2 to community 0"
    );
    assert!((size[0] - 2.0).abs() < 1e-10);
    assert!((size[2] - 0.0).abs() < 1e-10);
}

/// Verify apply_move across multiple layers updates all layer stats.
#[test]
fn test_apply_move_multilayer() {
    let mut partition = Partition::new(2);
    // 0→0, 1→1
    let mut total_degree: Vec<Vec<f64>> = vec![vec![3.0, 5.0], vec![1.0, 2.0]];
    let mut in_degree: Vec<Vec<f64>> = vec![vec![3.0, 5.0], vec![1.0, 2.0]];
    let mut size: Vec<f64> = vec![1.0, 1.0];

    apply_move(
        MoveTarget::Partition(&mut partition),
        1,
        1,
        0,
        NodeContribution {
            k_v_out: &[5.0, 2.0],
            k_v_in: &[5.0, 2.0],
            weight: 1.0,
        },
        &mut CommunityStats {
            total_degree_out: &mut total_degree,
            sigma_in: &mut in_degree,
            size: &mut size,
        },
    );

    assert_eq!(partition.community_of(1), 0);
    // Layer 0: comm 0 gains 5 → 8, comm 1 loses 5 → 0
    assert!((total_degree[0][0] - 8.0).abs() < 1e-10);
    assert!((total_degree[0][1] - 0.0).abs() < 1e-10);
    // Layer 1: comm 0 gains 2 → 3, comm 1 loses 2 → 0
    assert!((total_degree[1][0] - 3.0).abs() < 1e-10);
    assert!((total_degree[1][1] - 0.0).abs() < 1e-10);
    assert!((size[0] - 2.0).abs() < 1e-10);
    assert!((size[1] - 0.0).abs() < 1e-10);
}

// ── build_aggregated_graph tests ──

/// Verify that build_aggregated_graph creates the correct aggregated CSR.
///
/// Setup: 4-node graph forming two pairs (0-1, 2-3) with a bridge (1-2).
/// Partition: {0,1} → community 0, {2,3} → community 1.
/// The aggregated graph should have 2 super-nodes with correct edge weights.
#[test]
fn test_build_aggregated_graph_two_communities() {
    // Build original graph: 0-1, 1-2, 2-3 (chain)
    let mut b = GraphDataBuilder::new(4);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(1, 2, 1.0).unwrap();
    b.add_edge(2, 3, 1.0).unwrap();
    let graph = b.build().unwrap();

    // Partition: 0 and 1 in community 0; 2 and 3 in community 1
    // orig_to_agg maps: 0→0, 1→0, 2→1, 3→1
    let orig_to_agg: Vec<usize> = vec![0, 0, 1, 1];
    let agg_n = 2;

    // Collect edges via aggregate_node_edges_into
    let mut agg_edges_map: FxHashMap<(usize, usize), f64> = FxHashMap::default();
    for node in 0..4 {
        aggregate_node_edges_into(&graph, node, &orig_to_agg, false, &mut agg_edges_map);
    }

    // Build coarse partition matching orig_to_agg
    let mut coarse_partition = Partition::new(4);
    coarse_partition.move_node(0, 0);
    coarse_partition.move_node(1, 0);
    coarse_partition.move_node(2, 1);
    coarse_partition.move_node(3, 1);

    let (agg_data, returned_orig_to_agg, agg_initial) = build_aggregated_graph(
        orig_to_agg,
        agg_n,
        false, // undirected
        agg_edges_map,
        &coarse_partition,
        |node| graph.node_weight(node),
    )
    .unwrap();

    // 2 aggregate nodes
    assert_eq!(
        agg_data.node_count(),
        2,
        "aggregated graph should have 2 nodes"
    );

    // Node weights: each super-node has weight 2.0 (two original nodes of weight 1.0 each)
    assert!((agg_data.node_weight(0) - 2.0).abs() < 1e-10);
    assert!((agg_data.node_weight(1) - 2.0).abs() < 1e-10);

    // Edge between super-nodes: the bridge 1-2 maps to (0,1) with weight 1.0
    // Internal edges (0-1 and 2-3) become self-loops with weight 1.0 each
    let neighbors_0: Vec<(usize, f64)> = agg_data.neighbors(0).collect();
    let neighbors_1: Vec<(usize, f64)> = agg_data.neighbors(1).collect();

    // Super-node 0 has: self-loop (internal 0-1) weight 1.0 + edge to 1 (bridge 1-2) weight 1.0
    assert_eq!(
        neighbors_0.len(),
        2,
        "super-node 0 should have 2 neighbors (self + bridge)"
    );
    let has_self_0 = neighbors_0
        .iter()
        .any(|(n, w)| *n == 0 && (*w - 1.0).abs() < 1e-10);
    let has_bridge_from_0 = neighbors_0
        .iter()
        .any(|(n, w)| *n == 1 && (*w - 1.0).abs() < 1e-10);
    assert!(has_self_0, "super-node 0 should have self-loop weight 1.0");
    assert!(
        has_bridge_from_0,
        "super-node 0 should have edge to super-node 1 weight 1.0"
    );

    // Super-node 1 has: self-loop (internal 2-3) weight 1.0 + edge to 0 (bridge) weight 1.0
    assert_eq!(neighbors_1.len(), 2, "super-node 1 should have 2 neighbors");
    let has_self_1 = neighbors_1
        .iter()
        .any(|(n, w)| *n == 1 && (*w - 1.0).abs() < 1e-10);
    assert!(has_self_1, "super-node 1 should have self-loop weight 1.0");

    // Returned orig_to_agg should be preserved
    assert_eq!(returned_orig_to_agg, vec![0, 0, 1, 1]);

    // Aggregate initial partition should have communities matching coarse partition
    // Both agg nodes 0 and 1 should be in different communities (matching community 0 and 1)
    assert_eq!(agg_initial.community_of(0), 0);
    assert_eq!(agg_initial.community_of(1), 1);
}

/// Verify build_aggregated_graph with all nodes in one community produces a single super-node.
#[test]
fn test_build_aggregated_graph_single_community() {
    let mut b = GraphDataBuilder::new(3);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(1, 2, 1.0).unwrap();
    let graph = b.build().unwrap();

    // All nodes in community 0
    let orig_to_agg: Vec<usize> = vec![0, 0, 0];
    let agg_n = 1;

    let mut agg_edges_map: FxHashMap<(usize, usize), f64> = FxHashMap::default();
    for node in 0..3 {
        aggregate_node_edges_into(&graph, node, &orig_to_agg, false, &mut agg_edges_map);
    }

    let coarse_partition = Partition::new(3);
    // All in community 0 already (singleton partition, but all same community)

    let (agg_data, _, _) = build_aggregated_graph(
        orig_to_agg,
        agg_n,
        false,
        agg_edges_map,
        &coarse_partition,
        |node| graph.node_weight(node),
    )
    .unwrap();

    assert_eq!(
        agg_data.node_count(),
        1,
        "single community should produce 1 super-node"
    );
    assert!(
        (agg_data.node_weight(0) - 3.0).abs() < 1e-10,
        "weight should be 3.0"
    );

    // All edges become self-loops on the single super-node: 2 edges * 2.0 (undirected double-count) = 4.0
    // But aggregate_node_edges_into canonicalizes, so self-loop weight = sum of edge weights
    let neighbors: Vec<(usize, f64)> = agg_data.neighbors(0).collect();
    assert_eq!(
        neighbors.len(),
        1,
        "single super-node should only have self-loop"
    );
    assert_eq!(neighbors[0].0, 0, "neighbor should be self");
    assert!(
        (neighbors[0].1 - 2.0).abs() < 1e-10,
        "self-loop weight should be 2.0 (sum of edges)"
    );
}
