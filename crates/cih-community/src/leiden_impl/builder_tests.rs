use super::*;

#[test]
fn test_builder_triangle() {
    let mut b = GraphDataBuilder::new(3);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(1, 2, 2.0).unwrap();
    b.add_edge(0, 2, 3.0).unwrap();
    let gd = b.build().unwrap();

    assert_eq!(gd.node_count(), 3);
    // total_weight = (degree[0] + degree[1] + degree[2]) / 2
    // degree[0] = 1 + 3 = 4, degree[1] = 1 + 2 = 3, degree[2] = 2 + 3 = 5
    // total_weight = 12 / 2 = 6
    assert!((gd.total_weight() - 6.0).abs() < 1e-10);
    assert!((gd.degree_of(0) - 4.0).abs() < 1e-10);
    assert!((gd.degree_of(1) - 3.0).abs() < 1e-10);
    assert!((gd.degree_of(2) - 5.0).abs() < 1e-10);
}

#[test]
fn test_builder_self_loop() {
    let mut b = GraphDataBuilder::new(2);
    b.add_edge(0, 0, 5.0).unwrap();
    b.add_edge(0, 1, 1.0).unwrap();
    let gd = b.build().unwrap();

    // degree[0] = 2*5 + 1 = 11, degree[1] = 1, total_weight = 12 / 2 = 6
    assert_eq!(gd.node_count(), 2);
    assert!((gd.degree_of(0) - 11.0).abs() < 1e-10);
    assert!((gd.degree_of(1) - 1.0).abs() < 1e-10);
    assert!((gd.total_weight() - 6.0).abs() < 1e-10);
}

#[test]
fn test_builder_invalid_weight() {
    let mut b = GraphDataBuilder::new(3);
    assert!(b.add_edge(0, 1, f64::NAN).is_err());
    assert!(b.add_edge(0, 1, f64::INFINITY).is_err());
    assert!(b.add_edge(0, 1, -1.0).is_err());
}

#[test]
fn test_builder_node_out_of_range() {
    let mut b = GraphDataBuilder::new(3);
    assert!(b.add_edge(0, 5, 1.0).is_err());
    assert!(b.add_edge(5, 0, 1.0).is_err());
}

#[test]
fn test_builder_set_node_weight() {
    let mut b = GraphDataBuilder::new(3);
    b.set_node_weight(1, 5.0).unwrap();
    let gd = b.build().unwrap();
    assert!((gd.node_weight(0) - 1.0).abs() < 1e-10);
    assert!((gd.node_weight(1) - 5.0).abs() < 1e-10);
    assert!((gd.node_weight(2) - 1.0).abs() < 1e-10);
}

#[test]
fn test_builder_directed_basic() {
    let mut b = GraphDataBuilder::new(4).directed();
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(1, 2, 2.0).unwrap();
    b.add_edge(2, 0, 3.0).unwrap();
    b.add_edge(0, 3, 0.5).unwrap();
    let gd = b.build().unwrap();

    assert_eq!(gd.node_count(), 4);
    assert!(gd.is_directed());
    // total_weight = 1.0 + 2.0 + 3.0 + 0.5 = 6.5
    assert!((gd.total_weight() - 6.5).abs() < 1e-10);

    // out_degree: 0→1+0.5=1.5, 1→2=2, 2→3=3, 3→0=0
    assert!((gd.out_degree_of(0) - 1.5).abs() < 1e-10);
    assert!((gd.out_degree_of(1) - 2.0).abs() < 1e-10);
    assert!((gd.out_degree_of(2) - 3.0).abs() < 1e-10);
    assert!((gd.out_degree_of(3) - 0.0).abs() < 1e-10);

    // in_degree: 0→3=3, 1→1=1, 2→2=2, 3→0.5=0.5
    assert!((gd.in_degree_of(0) - 3.0).abs() < 1e-10);
    assert!((gd.in_degree_of(1) - 1.0).abs() < 1e-10);
    assert!((gd.in_degree_of(2) - 2.0).abs() < 1e-10);
    assert!((gd.in_degree_of(3) - 0.5).abs() < 1e-10);

    // degree_of for directed = out + in
    assert!((gd.degree_of(0) - 4.5).abs() < 1e-10);
    assert!((gd.degree_of(1) - 3.0).abs() < 1e-10);
}

#[test]
fn test_builder_directed_self_loop() {
    let mut b = GraphDataBuilder::new(3).directed();
    b.add_edge(0, 0, 5.0).unwrap();
    b.add_edge(0, 1, 1.0).unwrap();
    let gd = b.build().unwrap();

    // out_degree: 0→5+1=6, 1→0, 2→0
    assert!((gd.out_degree_of(0) - 6.0).abs() < 1e-10);
    // in_degree: 0→5, 1→1, 2→0
    assert!((gd.in_degree_of(0) - 5.0).abs() < 1e-10);
    assert!((gd.in_degree_of(1) - 1.0).abs() < 1e-10);
    // total_weight = 5.0 + 1.0 = 6.0
    assert!((gd.total_weight() - 6.0).abs() < 1e-10);
}

#[test]
fn test_builder_empty_graph() {
    let gd = GraphDataBuilder::new(5).build().unwrap();
    assert_eq!(gd.node_count(), 5);
    assert!((gd.total_weight() - 0.0).abs() < 1e-10);
    for i in 0..5 {
        assert!((gd.degree_of(i) - 0.0).abs() < 1e-10);
        assert_eq!(gd.neighbors(i).count(), 0);
    }
}

#[test]
fn test_duplicate_edges_merged_in_build() {
    let n = 3;
    let mut b = GraphDataBuilder::new(n);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(0, 1, 1.0).unwrap(); // duplicate — merged by summing
    let g = b.build().unwrap();
    // After merge: single edge (0,1,2.0), undirected: degree[0]=2, degree[1]=2, total=4/2=2
    assert!((g.total_weight() - 2.0).abs() < 1e-10);
    let nbrs: Vec<_> = g.neighbors(0).collect();
    assert_eq!(nbrs.len(), 1);
    assert!((nbrs[0].1 - 2.0).abs() < 1e-10);
}

#[test]
fn test_duplicate_edges_sum_weights() {
    let mut b = GraphDataBuilder::new(2);
    b.add_edge(0, 1, 1.0).unwrap();
    b.add_edge(0, 1, 2.0).unwrap();
    let g = b.build().unwrap();
    let nbrs: Vec<_> = g.neighbors(0).collect();
    assert_eq!(nbrs.len(), 1);
    assert!((nbrs[0].1 - 3.0).abs() < 1e-10);
}

#[test]
fn test_builder_matches_from_edgelist() {
    let edges: Vec<(usize, usize, f64)> =
        vec![(0, 1, 1.0), (1, 2, 2.0), (0, 2, 3.0), (2, 2, 0.5)];

    let mut b = GraphDataBuilder::new(3);
    for &(u, v, w) in &edges {
        b.add_edge(u, v, w).unwrap();
    }
    let gd = b.build().unwrap();

    let mut expected_degree = [0.0f64; 3];
    for &(u, v, w) in &edges {
        if u == v {
            expected_degree[u] += 2.0 * w;
        } else {
            expected_degree[u] += w;
            expected_degree[v] += w;
        }
    }
    let expected_total: f64 = expected_degree.iter().sum::<f64>() / 2.0;

    for (i, &expected) in expected_degree.iter().enumerate() {
        assert!(
            (gd.degree_of(i) - expected).abs() < 1e-10,
            "degree mismatch at node {i}"
        );
    }
    assert!(
        (gd.total_weight() - expected_total).abs() < 1e-10,
        "total_weight mismatch"
    );
}

#[test]
fn test_large_graph_float_precision() {
    let n = 2500;
    let mut b = GraphDataBuilder::new(n);
    for i in 0..n {
        b.add_edge(i, (i + 1) % n, 1.0 / 3.0).unwrap();
        b.add_edge(i, (i + 2) % n, 1.0 / 3.0).unwrap();
        b.add_edge(i, (i + 3) % n, 1.0 / 3.0).unwrap();
    }
    // Must not fail with InconsistentStructure
    let g = b.build().unwrap();
    for i in 0..n {
        let neighbors: Vec<_> = g.neighbors(i).collect();
        let row_sum: f64 = neighbors.iter().map(|&(_, w)| w).sum();
        let self_loop_sum: f64 = neighbors
            .iter()
            .filter(|&&(t, _)| t == i)
            .map(|&(_, w)| w)
            .sum();
        assert!(
            (g.degree_of(i) - (row_sum + self_loop_sum)).abs() < 1e-12,
            "degree mismatch at node {i}: degree={}, row_sum+slf={}",
            g.degree_of(i),
            row_sum + self_loop_sum
        );
    }
}

#[test]
fn test_large_directed_float_precision() {
    let n = 2500;
    let mut b = GraphDataBuilder::new(n).directed();
    for i in 0..n {
        b.add_edge(i, (i + 1) % n, 1.0 / 3.0).unwrap();
        b.add_edge(i, (i + 2) % n, 1.0 / 3.0).unwrap();
        b.add_edge(i, (i + 3) % n, 1.0 / 3.0).unwrap();
    }
    let g = b.build().unwrap();
    for i in 0..n {
        let out_nbrs: Vec<_> = g.out_neighbors(i).collect();
        let out_sum: f64 = out_nbrs.iter().map(|&(_, w)| w).sum();
        assert!(
            (g.out_degree_of(i) - out_sum).abs() < 1e-12,
            "out_degree mismatch at node {i}"
        );
        let in_nbrs: Vec<_> = g.in_neighbors(i).collect();
        let in_sum: f64 = in_nbrs.iter().map(|&(_, w)| w).sum();
        assert!(
            (g.in_degree_of(i) - in_sum).abs() < 1e-12,
            "in_degree mismatch at node {i}"
        );
    }
}
