use std::collections::{BTreeMap, HashMap, HashSet};

use cih_core::NodeKind;

use crate::graph::WikiGraph;
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
    let empty = Vec::new();
    let members = graph.members_by_community.get(community_id).unwrap_or(&empty);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for m in members {
        if let Some(feat) = feature_from_path(&m.file) {
            *counts.entry(feat.to_string()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, v)| *v)
        .map(|(k, _)| k)
        .unwrap_or_else(|| "shared".to_string())
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
/// `PaymentOrchestrationService` → `payment-orchestration-service`
pub fn pascal_to_kebab(name: &str) -> String {
    let mut result = String::new();
    for (i, ch) in name.char_indices() {
        if ch.is_uppercase() && i > 0 {
            result.push('-');
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
    let members = graph.members_by_community.get(community_id).unwrap_or(&empty);

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
        let simple = m
            .id
            .as_str()
            .split_once('#')
            .map(|(prefix, _)| {
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
            result.insert(
                comm_id.clone(),
                format!("{}/dev/{}", group.feature, slug),
            );
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn method_node(id: &str, file: &str) -> Node {
        let name = id
            .split('#')
            .nth(1)
            .and_then(|s| s.split('/').next())
            .unwrap_or("m")
            .to_string();
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Method,
            name,
            qualified_name: None,
            file: file.to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn comm_node(id: &str, name: &str) -> Node {
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

    fn member_edge(method: &str, comm: &str) -> Edge {
        Edge {
            src: NodeId::new(method.to_string()),
            dst: NodeId::new(comm.to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }
    }

    #[test]
    fn feature_inferred_from_modules_path() {
        let m = method_node(
            "Method:org.phuc.commerce.modules.payment.PaymentController#handleReturn/4",
            "src/main/java/org/phuc/commerce/modules/payment/PaymentController.java",
        );
        let comm = comm_node("Community:0", "Payment");
        let g = WikiGraph::build(
            &[m.clone()],
            &[],
            &[comm],
            &[member_edge(m.id.as_str(), "Community:0")],
        );
        assert_eq!(infer_community_feature("Community:0", &g), "payment");
    }

    #[test]
    fn feature_falls_back_to_shared() {
        let m = method_node("Method:com.example.Foo#bar/0", "Test.java");
        let comm = comm_node("Community:0", "misc");
        let g = WikiGraph::build(
            &[m.clone()],
            &[],
            &[comm],
            &[member_edge(m.id.as_str(), "Community:0")],
        );
        assert_eq!(infer_community_feature("Community:0", &g), "shared");
    }

    #[test]
    fn dev_slug_uses_primary_class_name() {
        let m = method_node(
            "Method:org.phuc.commerce.modules.payment.PaymentController#handleReturn/4",
            "src/main/java/org/phuc/commerce/modules/payment/PaymentController.java",
        );
        let comm = comm_node("Community:0", "Payment");
        let g = WikiGraph::build(
            &[m.clone()],
            &[],
            &[comm],
            &[member_edge(m.id.as_str(), "Community:0")],
        );
        let groups = group_communities_by_feature(&g);
        let paths = build_dev_page_paths(&groups, &g);
        assert_eq!(paths["Community:0"], "payment/dev/payment-controller");
    }

    #[test]
    fn slug_collision_gets_suffix() {
        let m1 = method_node(
            "Method:com.example.modules.order.OrderService#save/0",
            "src/main/java/com/example/modules/order/OrderService.java",
        );
        let m2 = method_node(
            "Method:com.example.modules.order.OrderService#find/0",
            "src/main/java/com/example/modules/order/OrderService.java",
        );
        let c1 = comm_node("Community:1", "Order");
        let c2 = comm_node("Community:2", "Order");
        let g = WikiGraph::build(
            &[m1.clone(), m2.clone()],
            &[],
            &[c1, c2],
            &[
                member_edge(m1.id.as_str(), "Community:1"),
                member_edge(m2.id.as_str(), "Community:2"),
            ],
        );
        let groups = group_communities_by_feature(&g);
        let paths = build_dev_page_paths(&groups, &g);
        let p1 = paths.get("Community:1").unwrap();
        let p2 = paths.get("Community:2").unwrap();
        assert_ne!(p1, p2, "paths must differ");
        assert!(
            p1 == "order/dev/order-service" || p2 == "order/dev/order-service",
            "one must have clean slug"
        );
    }

    #[test]
    fn pascal_to_kebab_converts_correctly() {
        assert_eq!(pascal_to_kebab("PaymentController"), "payment-controller");
        assert_eq!(
            pascal_to_kebab("PaymentOrchestrationService"),
            "payment-orchestration-service"
        );
        assert_eq!(pascal_to_kebab("PosOrderService"), "pos-order-service");
    }
}
