use super::*;
use crate::leiden_impl::builder::GraphDataBuilder;

fn undirected_mc() -> MoveComponents {
    MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 3.0,
        k_v_to_target_out: 2.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 3.0,
        k_v_in: 0.0,
        k_v_to_target_in: 0.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 0.0,
        sigma_tot_current_in: 0.0,
        n_target: 1.0,
        n_current: 1.0,
        directed: false,
    }
}

#[test]
fn test_graph_data_extraction() {
    let mut b = GraphDataBuilder::new(3);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(1, 2, 2.0).unwrap();
    let data = b.build().unwrap();
    assert_eq!(data.node_count(), 3);
    assert!((data.total_weight() - 3.0).abs() < 1e-10);
    assert!((data.degree_of(0) - 1.0).abs() < 1e-10);
    assert!((data.degree_of(1) - 3.0).abs() < 1e-10);
    assert!((data.degree_of(2) - 2.0).abs() < 1e-10);
}

#[test]
fn test_modularity_delta_positive() {
    let m = Modularity::new();
    let delta = m.delta_move_from_components(&undirected_mc());
    assert!(delta > 0.0);
}

#[test]
fn test_cpm_delta_positive() {
    let cpm = CPM::new(0.1);
    let delta = cpm.delta_move_from_components(&MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 3.0,
        k_v_to_target_out: 2.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 3.0,
        k_v_in: 0.0,
        k_v_to_target_in: 0.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 0.0,
        sigma_tot_current_in: 0.0,
        n_target: 5.0,
        n_current: 1.0,
        directed: false,
    });
    // delta = (2+0) - (0+0) - 0.1 * 1.0 * (5 - 1 + 1) = 2.0 - 0.5 = 1.5
    assert!((delta - 1.5).abs() < 1e-10);
}

#[test]
fn test_rbconfiguration_matches_modularity() {
    let rb = RBConfiguration::new();
    let m = Modularity::new();
    let c = undirected_mc();
    assert!(
        (rb.delta_move_from_components(&c) - m.delta_move_from_components(&c)).abs() < 1e-10
    );
}

#[test]
fn test_rbconfiguration_with_resolution() {
    let rb = RBConfiguration::with_resolution(2.0);
    let m = Modularity::with_resolution(2.0);
    let c = MoveComponents {
        two_m: 30.0,
        node_weight: 1.0,
        total_node_weight: 20.0,
        k_v_out: 5.0,
        k_v_to_target_out: 3.0,
        k_v_to_current_out: 1.0,
        sigma_tot_target_out: 15.0,
        sigma_tot_current_out: 8.0,
        k_v_in: 0.0,
        k_v_to_target_in: 0.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 0.0,
        sigma_tot_current_in: 0.0,
        n_target: 3.0,
        n_current: 2.0,
        directed: false,
    };
    assert!(
        (rb.delta_move_from_components(&c) - m.delta_move_from_components(&c)).abs() < 1e-10
    );
}

#[test]
fn test_rber_delta_positive() {
    let rber = RBER::new(1.0);
    let c = MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 5.0,
        k_v_to_target_out: 4.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 5.0,
        k_v_in: 0.0,
        k_v_to_target_in: 0.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 0.0,
        sigma_tot_current_in: 0.0,
        n_target: 5.0,
        n_current: 1.0,
        directed: false,
    };
    let delta = rber.delta_move_from_components(&c);
    assert!(delta > 0.0, "RBER delta should be positive, got {delta}");
}

#[test]
fn test_rber_delta_calculation() {
    let rber = RBER::new(1.0);
    // p = 20 / (10 * 9) = 0.2222...
    // delta = (4+0 - 0+0) - 1.0 * 0.2222 * 1.0 * (5 - 1 + 1) = 4 - 1.111 = 2.889
    let c = MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 5.0,
        k_v_to_target_out: 4.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 5.0,
        k_v_in: 0.0,
        k_v_to_target_in: 0.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 0.0,
        sigma_tot_current_in: 0.0,
        n_target: 5.0,
        n_current: 1.0,
        directed: false,
    };
    let delta = rber.delta_move_from_components(&c);
    let p = 20.0 / (10.0 * 9.0);
    let expected = 4.0 - 1.0 * p * 1.0 * (5.0 - 1.0 + 1.0);
    assert!(
        (delta - expected).abs() < 1e-10,
        "expected {expected}, got {delta}"
    );
}

#[test]
fn test_rber_zero_two_m() {
    let rber = RBER::new(1.0);
    let c = MoveComponents {
        two_m: 0.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 0.0,
        k_v_to_target_out: 0.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 0.0,
        sigma_tot_current_out: 0.0,
        k_v_in: 0.0,
        k_v_to_target_in: 0.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 0.0,
        sigma_tot_current_in: 0.0,
        n_target: 1.0,
        n_current: 1.0,
        directed: false,
    };
    assert!((rber.delta_move_from_components(&c)).abs() < 1e-10);
}

#[test]
fn test_modularity_directed_delta() {
    let m = Modularity::new();
    let c = MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 3.0,
        k_v_to_target_out: 2.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 3.0,
        k_v_in: 2.0,
        k_v_to_target_in: 1.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 8.0,
        sigma_tot_current_in: 2.0,
        n_target: 1.0,
        n_current: 1.0,
        directed: true,
    };
    let delta = m.delta_move_from_components(&c);
    // m = 10.0
    // d_internal = (2+1) - (0+0) = 3.0
    // d_expected = 2.0*(10-3) + 3.0*(8-2) + 2*3*2 = 14 + 18 + 12 = 44
    // delta = 3.0/10.0 - 1.0 * 44.0/100.0 = 0.3 - 0.44 = -0.14
    let expected = 3.0 / 10.0 - 44.0 / 100.0;
    assert!(
        (delta - expected).abs() < 1e-10,
        "expected {expected}, got {delta}"
    );
}

#[test]
fn test_cpm_directed_delta() {
    let cpm = CPM::new(0.1);
    let c = MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 3.0,
        k_v_to_target_out: 2.0,
        k_v_to_current_out: 1.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 3.0,
        k_v_in: 2.0,
        k_v_to_target_in: 1.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 8.0,
        sigma_tot_current_in: 2.0,
        n_target: 5.0,
        n_current: 1.0,
        directed: true,
    };
    let delta = cpm.delta_move_from_components(&c);
    // (2+1) - (1+0) - 0.1*1.0*(5-1+1) = 3 - 1 - 0.5 = 1.5
    assert!((delta - 1.5).abs() < 1e-10);
}

#[test]
fn test_rbconfiguration_directed_matches_modularity() {
    let rb = RBConfiguration::new();
    let m = Modularity::new();
    let c = MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 3.0,
        k_v_to_target_out: 2.0,
        k_v_to_current_out: 0.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 3.0,
        k_v_in: 2.0,
        k_v_to_target_in: 1.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 8.0,
        sigma_tot_current_in: 2.0,
        n_target: 1.0,
        n_current: 1.0,
        directed: true,
    };
    assert!(
        (rb.delta_move_from_components(&c) - m.delta_move_from_components(&c)).abs() < 1e-10
    );
}

#[test]
fn test_rber_directed_delta() {
    let rber = RBER::new(1.0);
    let c = MoveComponents {
        two_m: 20.0,
        node_weight: 1.0,
        total_node_weight: 10.0,
        k_v_out: 5.0,
        k_v_to_target_out: 4.0,
        k_v_to_current_out: 1.0,
        sigma_tot_target_out: 10.0,
        sigma_tot_current_out: 5.0,
        k_v_in: 3.0,
        k_v_to_target_in: 2.0,
        k_v_to_current_in: 0.0,
        sigma_tot_target_in: 8.0,
        sigma_tot_current_in: 3.0,
        n_target: 5.0,
        n_current: 1.0,
        directed: true,
    };
    let delta = rber.delta_move_from_components(&c);
    let p = 20.0 / (10.0 * 9.0);
    let expected = (4.0 + 2.0) - (1.0 + 0.0) - 1.0 * p * 1.0 * (5.0 - 1.0 + 1.0);
    assert!(
        (delta - expected).abs() < 1e-10,
        "expected {expected}, got {delta}"
    );
}
