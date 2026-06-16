use std::collections::HashMap;

use crate::graph::WikiGraph;

fn capitalize(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// Render the feature landing page that links to po/ba pages and lists dev modules.
pub fn render_feature_index(
    feature: &str,
    community_ids: &[String],
    dev_paths: &HashMap<String, String>,
    graph: &WikiGraph,
) -> String {
    let title = format!("{} — Feature Overview", capitalize(feature));
    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\n---\n\n",
        title
    ));
    md.push_str(&format!("# {}\n\n", title));

    let total_routes: usize = community_ids
        .iter()
        .map(|cid| {
            graph
                .community_routes
                .get(cid)
                .map(|r| r.len())
                .unwrap_or(0)
        })
        .sum();
    let total_methods: usize = community_ids
        .iter()
        .map(|cid| graph.community_method_counts.get(cid).copied().unwrap_or(0))
        .sum();

    md.push_str(&format!(
        "**Modules:** {} · **Routes:** {} · **Methods:** {}\n\n",
        community_ids.len(),
        total_routes,
        total_methods,
    ));

    md.push_str("## Role Pages\n\n");
    md.push_str(&format!("- [Business Overview]({feature}/po.md)\n"));
    md.push_str(&format!("- [Business Analysis]({feature}/ba.md)\n\n"));

    md.push_str("## Technical Modules\n\n");
    md.push_str("| Module | Routes | Methods | Dev Page |\n");
    md.push_str("|---|---|---|---|\n");
    for cid in community_ids {
        let comm_name = graph.community_name(cid);
        let route_count = graph
            .community_routes
            .get(cid)
            .map(|r| r.len())
            .unwrap_or(0);
        let method_count = graph.community_method_counts.get(cid).copied().unwrap_or(0);
        let dev_path = dev_paths.get(cid).cloned().unwrap_or_default();
        md.push_str(&format!(
            "| {} | {} | {} | [dev]({}.md) |\n",
            comm_name, route_count, method_count, dev_path
        ));
    }
    md.push('\n');

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn simple_graph() -> WikiGraph {
        let m = Node {
            id: NodeId::new("Method:A#do/0".to_string()),
            kind: NodeKind::Method,
            name: "do".to_string(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        };
        let c = Node {
            id: NodeId::new("Community:0".to_string()),
            kind: NodeKind::Community,
            name: "order-service".to_string(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        };
        WikiGraph::build(
            &[m.clone()],
            &[],
            &[c],
            &[Edge {
                src: m.id.clone(),
                dst: NodeId::new("Community:0".to_string()),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            }],
        )
    }

    #[test]
    fn renders_with_correct_frontmatter() {
        let g = simple_graph();
        let ids = vec!["Community:0".to_string()];
        let mut dev_paths = HashMap::new();
        dev_paths.insert("Community:0".to_string(), "order/dev/order-service".to_string());
        let md = render_feature_index("order", &ids, &dev_paths, &g);
        assert!(md.contains("---\ntitle: Order — Feature Overview"));
        assert!(md.contains("Order — Feature Overview"));
        assert!(md.contains("order-service"));
        assert!(md.contains("order/dev/order-service.md"));
    }
}
