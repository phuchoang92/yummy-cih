use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cih_core::NodeKind;

use crate::graph::{route_path, WikiGraph};
use crate::slugify::slugify;

/// A group of communities that belong to the same feature/module.
#[derive(Debug, Clone)]
pub struct FeatureGroup {
    /// Feature slug, e.g. "payment", "order", "shared".
    pub feature: String,
    /// Community IDs sorted for determinism.
    pub community_ids: Vec<String>,
}

/// Extract the feature name from a Java file path by looking for `modules/<feature>/`.
fn feature_from_path(path: &str) -> Option<&str> {
    let prefix = "modules/";
    let start = path.find(prefix)?;
    let rest = &path[start + prefix.len()..];
    let end = rest.find('/')?;
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Infer the dominant feature for a community from its member node file paths.
pub fn infer_community_feature(community_id: &str, graph: &WikiGraph) -> String {
    // Fast-path: prefer enriched prop written by cih-community
    if let Some(feature) = graph
        .nodes_by_id
        .get(community_id)
        .and_then(|n| n.props.as_ref())
        .and_then(|p| p.get("feature"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "shared")
    {
        return feature.to_string();
    }

    let empty = Vec::new();
    let members = graph
        .members_by_community
        .get(community_id)
        .unwrap_or(&empty);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for m in members {
        if let Some(feat) = feature_from_path(&m.file) {
            *counts.entry(feat.to_string()).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        if let Some(routes) = graph.community_routes.get(community_id) {
            for (_, route) in routes {
                if let Some(feature) = route_feature(&route_path(route)) {
                    *counts.entry(feature).or_insert(0) += 1;
                }
            }
        }
    }
    if counts.is_empty() {
        if let Some(tables) = graph.community_db_tables.get(community_id) {
            for table in tables {
                if let Some(feature) = table_feature(&table.table_name) {
                    *counts.entry(feature).or_insert(0) += 1;
                }
            }
        }
    }
    if counts.is_empty() {
        for member in members {
            for topic_id in graph
                .publishes
                .get(member.id.as_str())
                .into_iter()
                .flatten()
                .chain(graph.listens.get(member.id.as_str()).into_iter().flatten())
            {
                if let Some(feature) = topic_feature(topic_id) {
                    *counts.entry(feature).or_insert(0) += 1;
                }
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, v)| *v)
        .map(|(k, _)| k)
        .unwrap_or_else(|| "shared".to_string())
}

fn route_feature(path: &str) -> Option<String> {
    path.split('/')
        .filter_map(clean_feature_token)
        .find(|token| !is_generic_route_token(token))
}

fn table_feature(table: &str) -> Option<String> {
    table
        .split(|ch: char| ch == '_' || ch == '.' || ch == '-' || ch == '/')
        .filter_map(clean_feature_token)
        .find(|token| !is_generic_route_token(token))
}

fn topic_feature(topic_id: &str) -> Option<String> {
    let topic = topic_id.strip_prefix("KafkaTopic:").unwrap_or(topic_id);
    topic
        .split(|ch: char| ch == '_' || ch == '.' || ch == '-' || ch == '/')
        .filter_map(clean_feature_token)
        .find(|token| !is_generic_route_token(token))
}

fn clean_feature_token(raw: &str) -> Option<String> {
    let token = raw.trim();
    if token.is_empty()
        || token.starts_with('{')
        || token.chars().all(|ch| ch.is_ascii_digit())
        || is_version_segment(token)
    {
        return None;
    }
    let slug = slugify(token);
    if slug == "community" {
        None
    } else {
        Some(slug)
    }
}

fn is_version_segment(token: &str) -> bool {
    let mut chars = token.chars();
    matches!(chars.next(), Some('v') | Some('V')) && chars.all(|ch| ch.is_ascii_digit())
}

fn is_generic_route_token(token: &str) -> bool {
    matches!(
        token,
        "api"
            | "apis"
            | "rest"
            | "internal"
            | "external"
            | "service"
            | "services"
            | "common"
            | "shared"
            | "core"
            | "app"
            | "apps"
    )
}

/// Group by Java package path (package-grouping mode).
/// Each `Pkg:<feature>` synthetic community node maps 1:1 to a FeatureGroup.
pub fn group_nodes_by_package(graph: &WikiGraph) -> Vec<FeatureGroup> {
    graph
        .community_nodes
        .iter()
        .filter(|n| n.id.as_str().starts_with("Pkg:"))
        .map(|n| FeatureGroup {
            feature: n.name.clone(),
            community_ids: vec![n.id.as_str().to_string()],
        })
        .collect()
}

/// Group all communities in the graph by their dominant feature.
/// Features are sorted alphabetically; communities within each feature by id.
pub fn group_communities_by_feature(graph: &WikiGraph) -> Vec<FeatureGroup> {
    let mut feature_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for comm in &graph.community_nodes {
        let feat = infer_community_feature(comm.id.as_str(), graph);
        feature_map
            .entry(feat)
            .or_default()
            .push(comm.id.as_str().to_string());
    }
    // Sort community IDs within each feature for determinism
    for ids in feature_map.values_mut() {
        ids.sort();
    }
    feature_map
        .into_iter()
        .map(|(feature, community_ids)| FeatureGroup {
            feature,
            community_ids,
        })
        .collect()
}

/// Convert a PascalCase class name to a kebab-case slug.
/// Handles acronyms correctly: `ProgressiveEMICalculator` → `progressive-emi-calculator`,
/// `PaymentOrchestrationService` → `payment-orchestration-service`.
/// Rule: insert `-` before an uppercase letter when the previous char is lowercase,
/// OR when the previous char is uppercase but the next char is lowercase (end of acronym).
pub fn pascal_to_kebab(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut result = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            let prev_lower = prev.is_lowercase();
            let next_lower = next.map(|c| c.is_lowercase()).unwrap_or(false);
            if prev_lower || (prev.is_uppercase() && next_lower) {
                result.push('-');
            }
        }
        result.push(ch.to_ascii_lowercase());
    }
    if result.is_empty() {
        "class".to_string()
    } else {
        result
    }
}

fn is_test_class(name: &str, file: &str) -> bool {
    file.contains("/test/")
        || name.ends_with("Test")
        || name.ends_with("Tests")
        || name.ends_with("IT")
        || name.ends_with("Spec")
}

/// Compute the base dev slug for a community (before collision deduplication).
fn primary_class_slug_base(community_id: &str, graph: &WikiGraph, feature: &str) -> String {
    let empty = Vec::new();
    let members = graph
        .members_by_community
        .get(community_id)
        .unwrap_or(&empty);

    let mut classes: Vec<(String, bool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for m in members {
        if !matches!(
            m.kind,
            NodeKind::Method | NodeKind::Function | NodeKind::Constructor
        ) {
            continue;
        }
        // Extract simple class name from method id: "Method:pkg.ClassName#method/arity"
        let simple = m.id.as_str().split_once('#').map(|(prefix, _)| {
            prefix
                .trim_start_matches("Method:")
                .trim_start_matches("Constructor:")
                .trim_start_matches("Function:")
                .rsplit('.')
                .next()
                .unwrap_or("Unknown")
                .to_string()
        });
        if let Some(name) = simple {
            if seen.insert(name.clone()) {
                let is_test = is_test_class(&name, &m.file);
                classes.push((name, is_test));
            }
        }
    }

    let non_test: Vec<_> = classes.iter().filter(|(_, t)| !t).collect();
    let candidates: Vec<_> = if non_test.is_empty() {
        classes.iter().collect()
    } else {
        non_test
    };

    if candidates.is_empty() {
        return graph
            .community_nodes
            .iter()
            .find(|n| n.id.as_str() == community_id)
            .map(|n| slugify(&n.name))
            .unwrap_or_else(|| "community".to_string());
    }

    // Prefer the class whose name contains the feature name
    let feat_lower = feature.to_lowercase();
    let best = candidates
        .iter()
        .find(|(name, _)| name.to_lowercase().contains(&feat_lower))
        .or_else(|| candidates.first())
        .unwrap();

    pascal_to_kebab(&best.0)
}

/// Build a map from `community_id` → full dev page path,
/// e.g. `"Community:157"` → `"payment/dev/payment-controller"`.
/// Handles slug collisions within a feature by appending `-2`, `-3`, etc.
pub fn build_dev_page_paths(
    feature_groups: &[FeatureGroup],
    graph: &WikiGraph,
) -> HashMap<String, String> {
    let mut result = HashMap::new();

    for group in feature_groups {
        let mut usage: BTreeMap<String, usize> = BTreeMap::new();

        for comm_id in &group.community_ids {
            let base = primary_class_slug_base(comm_id, graph, &group.feature);
            let count = usage.entry(base.clone()).or_insert(0);
            *count += 1;
            let slug = if *count == 1 {
                base
            } else {
                format!("{}-{}", base, count)
            };
            result.insert(comm_id.clone(), format!("{}/dev/{}", group.feature, slug));
        }
    }

    result
}

/// Assign dev-page slugs to a sorted set of class IDs using a two-pass collision counter.
///
/// `get_name` returns the simple class name for a given class_id; used to compute the base
/// kebab slug. When two class IDs in the same set produce the same base slug, `-2`, `-3`, …
/// are appended in BTreeSet iteration order so the result is fully deterministic.
///
/// Both `lib.rs` page generation and `wiki_cmd.rs` citation-map building must call this
/// function with the same `class_ids` set to guarantee they produce identical slugs.
/// Assign dev-page slugs to a sorted set of class IDs using a two-pass collision counter.
///
/// `get_name` returns the simple class name for a given class_id (returned as an owned
/// `String` to avoid lifetime constraints on borrows from external maps). When two class
/// IDs in the same set produce the same base kebab slug, `-2`, `-3`, … are appended in
/// BTreeSet iteration order so the result is fully deterministic.
///
/// Both `lib.rs` page generation and `wiki_cmd.rs` citation-map building must call this
/// function with the same `class_ids` set to guarantee they produce identical slugs.
pub fn assign_class_slugs<F>(class_ids: &BTreeSet<String>, get_name: F) -> HashMap<String, String>
where
    F: Fn(&str) -> String,
{
    // Pass 1: count how many IDs share each base slug.
    let mut slug_counts: BTreeMap<String, usize> = BTreeMap::new();
    for id in class_ids {
        let base = pascal_to_kebab(&get_name(id.as_str()));
        *slug_counts.entry(base).or_insert(0) += 1;
    }
    // Pass 2: assign final slugs (BTreeSet guarantees sorted/deterministic order).
    let mut slug_assign: BTreeMap<String, usize> = BTreeMap::new();
    let mut result: HashMap<String, String> = HashMap::with_capacity(class_ids.len());
    for id in class_ids {
        let base = pascal_to_kebab(&get_name(id.as_str()));
        let n = slug_counts.get(&base).copied().unwrap_or(1);
        let idx = slug_assign.entry(base.clone()).or_insert(0);
        *idx += 1;
        let slug = if n == 1 {
            base
        } else {
            format!("{}-{}", base, idx)
        };
        result.insert(id.clone(), slug);
    }
    result
}


