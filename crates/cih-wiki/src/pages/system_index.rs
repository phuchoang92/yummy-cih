use std::collections::HashSet;

use crate::features::FeatureGroup;
use crate::graph::WikiGraph;

fn capitalize(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// Render the top-level system index page (pages/index.md).
/// Lists all features with their module, route, and table counts.
pub fn render_system_index(
    feature_groups: &[FeatureGroup],
    graph: &WikiGraph,
    repo_name: &str,
) -> String {
    let mut md = String::new();
    md.push_str(&format!("---\nslug: /\ntitle: {}\n---\n\n", repo_name));
    md.push_str(&format!("# {}\n\n", repo_name));
    md.push_str(&format!(
        "**Features:** {} · **Modules:** {} · **Routes:** {}\n\n",
        feature_groups.len(),
        graph.community_nodes.len(),
        graph.routes.len(),
    ));

    md.push_str("## Features\n\n");
    md.push_str("| Feature | Modules | Routes | Tables |\n");
    md.push_str("|---|---|---|---|\n");

    for group in feature_groups {
        let module_count = group.community_ids.len();
        let route_count: usize = group
            .community_ids
            .iter()
            .map(|cid| {
                graph
                    .community_routes
                    .get(cid)
                    .map(|r| r.len())
                    .unwrap_or(0)
            })
            .sum();
        let mut tables: HashSet<String> = HashSet::new();
        for cid in &group.community_ids {
            if let Some(ts) = graph.community_db_tables.get(cid) {
                for t in ts {
                    tables.insert(t.table_name.clone());
                }
            }
        }
        md.push_str(&format!(
            "| [{}]({}/index.md) | {} | {} | {} |\n",
            capitalize(&group.feature),
            group.feature,
            module_count,
            route_count,
            tables.len(),
        ));
    }
    md.push('\n');

    if !graph.inter_community_calls.is_empty() {
        md.push_str("## Cross-Module Dependencies\n\n");
        md.push_str("| Caller | Callee | Calls |\n");
        md.push_str("|---|---|---|\n");
        for (src, dst, count) in &graph.inter_community_calls {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                graph.community_name(src),
                graph.community_name(dst),
                count,
            ));
        }
        md.push('\n');
    }

    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
    use crate::features::FeatureGroup;

    fn simple_setup() -> (WikiGraph, Vec<FeatureGroup>) {
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
            name: "order".to_string(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        };
        let g = WikiGraph::build(
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
        );
        let groups = vec![FeatureGroup {
            feature: "order".to_string(),
            community_ids: vec!["Community:0".to_string()],
        }];
        (g, groups)
    }

    #[test]
    fn renders_repo_name_and_feature_table() {
        let (g, groups) = simple_setup();
        let md = render_system_index(&groups, &g, "my-service");
        assert!(md.contains("---\nslug: /\ntitle: my-service"));
        assert!(md.contains("## Features"));
        assert!(md.contains("[Order](order/index.md)"));
    }
}
