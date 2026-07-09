use super::*;

#[test]
fn from_membership_empty() {
    let p = Partition::from_membership(vec![0usize; 0]);
    assert_eq!(p.num_communities(), 0);
    assert!(p.community.is_empty());
}

#[test]
fn from_membership_singleton() {
    let p = Partition::from_membership(vec![0]);
    assert_eq!(p.num_communities(), 1);
    assert_eq!(p.community_of(0), 0);
}

#[test]
fn from_membership_multiple_communities() {
    let p = Partition::from_membership(vec![0, 0, 1, 1, 2]);
    assert_eq!(p.num_communities(), 3);
    assert_eq!(p.nodes_in_community(0), vec![0, 1]);
    assert_eq!(p.nodes_in_community(1), vec![2, 3]);
    assert_eq!(p.nodes_in_community(2), vec![4]);
}

#[test]
fn community_sizes_empty() {
    let p = Partition::from_membership(vec![0usize; 0]);
    assert_eq!(p.community_sizes(), Vec::<usize>::new());
}

#[test]
fn community_sizes_all_in_one() {
    let p = Partition::from_membership(vec![0, 0, 0]);
    assert_eq!(p.community_sizes(), vec![3]);
}

#[test]
fn nodes_in_community_gap() {
    // Community 1 exists in the range (0..3) but has no members
    let p = Partition::from_membership(vec![0, 0, 2]);
    assert_eq!(p.nodes_in_community(1), Vec::<usize>::new());
}

#[test]
fn nodes_in_community_out_of_range() {
    // Query a community ID beyond the partition's range
    let p = Partition::from_membership(vec![0, 0, 1, 1]);
    assert_eq!(p.nodes_in_community(99), Vec::<usize>::new());
}
