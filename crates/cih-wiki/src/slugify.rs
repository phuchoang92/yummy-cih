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
