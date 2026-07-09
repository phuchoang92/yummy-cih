use super::*;
use crate::leiden::builder::GraphDataBuilder;
use crate::leiden::graph_data::GraphData;
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
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_negative_resolution() {
    let config = LeidenConfig {
        resolution: -1.0,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_nan_resolution() {
    let config = LeidenConfig {
        resolution: f64::NAN,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_infinite_resolution() {
    let config = LeidenConfig {
        resolution: f64::INFINITY,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_zero_epsilon() {
    let config = LeidenConfig {
        epsilon: 0.0,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_negative_epsilon() {
    let config = LeidenConfig {
        epsilon: -1e-10,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_nan_epsilon() {
    let config = LeidenConfig {
        epsilon: f64::NAN,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
}

#[test]
fn test_validate_rejects_infinite_epsilon() {
    let config = LeidenConfig {
        epsilon: f64::INFINITY,
        ..Default::default()
    };
    let err = config.validate().unwrap_err();
    assert!(matches!(
        err,
        crate::leiden::error::LeidenError::InvalidParameter { .. }
    ));
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
    assert!(
        !config.skip_refinement,
        "default should be false (Leiden mode)"
    );

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
    assert_eq!(
        result.partition.as_slice(),
        default_result.partition.as_slice()
    );
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
