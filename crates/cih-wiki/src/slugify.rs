use cih_core::Node;
use std::collections::{BTreeMap, BTreeSet};

/// Convert an arbitrary name into a URL-safe slug.
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "community".to_string()
    } else {
        slug
    }
}

/// Build a stable `community_id → slug` map.
/// Nodes are sorted by id for determinism. Slug collisions are resolved by
/// appending the slugified community id to the base slug.
pub fn build_slug_map(community_nodes: &[Node]) -> BTreeMap<String, String> {
    let mut sorted: Vec<&Node> = community_nodes.iter().collect();
    sorted.sort_by_key(|n| n.id.as_str());

    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut result: BTreeMap<String, String> = BTreeMap::new();

    for node in sorted {
        let base = slugify(&node.name);
        let slug = if used.contains(&base) {
            let id_slug = slugify(node.id.as_str());
            let candidate = format!("{}-{}", base, id_slug);
            candidate
        } else {
            base.clone()
        };
        used.insert(slug.clone());
        result.insert(node.id.as_str().to_string(), slug);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{NodeId, NodeKind, Range};

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
}
