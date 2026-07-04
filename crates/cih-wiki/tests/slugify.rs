use cih_core::{NodeId, NodeKind, Range};
use cih_core::Node;
use cih_wiki::slugify::{build_slug_map, slugify};
use std::collections::BTreeSet;

fn make_community(id: &str, name: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Community,
        name: name.to_string(),
        qualified_name: None,
        file: String::new(),
        range: Range::default(),
        props: None,
    }
}

#[test]
fn slugify_converts_community_names() {
    assert_eq!(slugify("Order Service"), "order-service");
    assert_eq!(slugify("order-service"), "order-service");
    assert_eq!(slugify("Order@Service"), "order-service");
    assert_eq!(slugify("  payment gateway  "), "payment-gateway");
    assert_eq!(slugify(""), "community");
    assert_eq!(slugify("---"), "community");
    assert_eq!(slugify("ABC123"), "abc123");
}

#[test]
fn slugify_handles_collisions() {
    let nodes = vec![
        make_community("Community:1", "Order Service"),
        make_community("Community:3", "order-service"),
        make_community("Community:5", "Order@Service"),
    ];
    let map = build_slug_map(&nodes);
    // Community:1 is first in id sort, gets base slug
    assert_eq!(map["Community:1"], "order-service");
    // Remaining collisions get suffixed with community id slug
    let slug_3 = &map["Community:3"];
    assert!(
        slug_3.starts_with("order-service-"),
        "expected suffix, got: {}",
        slug_3
    );
    let slug_5 = &map["Community:5"];
    assert!(
        slug_5.starts_with("order-service-"),
        "expected suffix, got: {}",
        slug_5
    );
    // All three slugs must be distinct
    let slugs: BTreeSet<_> = map.values().collect();
    assert_eq!(slugs.len(), 3);
}
