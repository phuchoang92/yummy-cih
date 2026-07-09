use super::*;
use crate::leiden::builder::GraphDataBuilder;

fn make_graph() -> GraphData {
    let mut b = GraphDataBuilder::new(3);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(1, 2, 2.0).unwrap();
    b.build().unwrap()
}

/// `in_neighbors(n)` with OOB node — returns empty (already guarded).
#[test]
fn in_neighbors_ok_on_oob_node() {
    let g = make_graph();
    let v: Vec<_> = g.in_neighbors(3).collect();
    assert!(v.is_empty());
}

/// `in_neighbor_slices(n)` should NOT panic — already guarded.
#[test]
fn in_neighbor_slices_ok_on_oob_node() {
    let g = make_graph();
    let (t, w) = g.in_neighbor_slices(3);
    assert!(t.is_empty());
    assert!(w.is_empty());
}

/// `in_degree_of(n)` should NOT panic — already guarded.
#[test]
fn in_degree_of_ok_on_oob_node() {
    let g = make_graph();
    assert_eq!(g.in_degree_of(3), 0.0);
}

// ── GREEN tests: these will pass after fixing ──

#[test]
fn neighbors_returns_empty_for_oob() {
    let g = make_graph();
    let v: Vec<_> = g.neighbors(3).collect();
    assert!(v.is_empty());
    let v2: Vec<_> = g.neighbors(usize::MAX).collect();
    assert!(v2.is_empty());
}

#[test]
fn neighbor_slices_returns_empty_for_oob() {
    let g = make_graph();
    let (t, w) = g.neighbor_slices(3);
    assert!(t.is_empty());
    assert!(w.is_empty());
}

#[test]
fn degree_of_returns_zero_for_oob() {
    let g = make_graph();
    assert_eq!(g.degree_of(3), 0.0);
}

#[test]
fn node_weight_returns_zero_for_oob() {
    let g = make_graph();
    assert_eq!(g.node_weight(3), 0.0);
}

#[test]
fn out_neighbors_returns_empty_for_oob() {
    let g = make_graph();
    let v: Vec<_> = g.out_neighbors(3).collect();
    assert!(v.is_empty());
}

#[test]
fn out_neighbor_slices_returns_empty_for_oob() {
    let g = make_graph();
    let (t, w) = g.out_neighbor_slices(3);
    assert!(t.is_empty());
    assert!(w.is_empty());
}

#[test]
fn out_degree_of_returns_zero_for_oob() {
    let g = make_graph();
    assert_eq!(g.out_degree_of(3), 0.0);
}

// ── Regression: valid node IDs still work ──

#[test]
fn valid_neighbors_still_work() {
    let g = make_graph();
    let v: Vec<_> = g.neighbors(0).collect();
    assert_eq!(v, vec![(1, 1.0)]);
    let v2: Vec<_> = g.neighbors(1).collect();
    // node 1: edge to 0 (via out-edge store) and to 2
    assert_eq!(v2.len(), 2);
}

#[test]
fn valid_neighbor_slices_still_work() {
    let g = make_graph();
    let (t, w) = g.neighbor_slices(0);
    assert_eq!(t.len(), 1);
    assert_eq!(w.len(), 1);
}

#[test]
fn valid_degree_of_still_works() {
    let g = make_graph();
    // node 0: degree = 1.0 (weight to node 1)
    assert_eq!(g.degree_of(0), 1.0);
}

#[test]
fn valid_node_weight_still_works() {
    let g = make_graph();
    assert_eq!(g.node_weight(0), 1.0);
}
