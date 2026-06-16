use std::collections::BTreeMap;
use cih_core::{Node, NodeKind, RepoMap};
use crate::graph::{node_stereotype, route_http_method, route_path, WikiGraph};
use crate::CommunityLlmSummary;

pub fn render_dev_index(
    graph: &WikiGraph,
    repo_map: Option<&RepoMap>,
    unresolved_report: Option<&str>,
) -> String {
    let mut md = String::new();
    md.push_str("---\nid: dev/index\ntitle: Technical Overview\n---\n\n");
    md.push_str("# Technical Overview\n\n");

    md.push_str("## Community Summary\n\n");
    md.push_str("| Module | Classes | Methods | Routes | Tests |\n");
    md.push_str("|---|---|---|---|---|\n");

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let classes = graph.community_class_counts.get(comm_id).copied().unwrap_or(0);
        let methods = graph.community_method_counts.get(comm_id).copied().unwrap_or(0);
        let routes = graph.community_routes.get(comm_id).map(|r| r.len()).unwrap_or(0);
        let tests = graph.community_tests.get(comm_id).map(|t| t.len()).unwrap_or(0);
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            comm.name, classes, methods, routes, tests,
        ));
    }
    md.push('\n');

    if let Some(rm) = repo_map {
        if !rm.modules.is_empty() {
            md.push_str("## Modules\n\n");
            md.push_str("| Module | Path |\n");
            md.push_str("|---|---|\n");
            for m in &rm.modules {
                md.push_str(&format!("| `{}` | `{}` |\n", m.name, m.rel_path));
            }
            md.push('\n');
        }

        if !rm.jars.is_empty() {
            md.push_str("## JAR Dependencies\n\n");
            md.push_str(&format!("{} external JARs detected.\n\n", rm.jars.len()));
        }
    }

    if let Some(report) = unresolved_report {
        md.push_str("## Unresolved References\n\n");
        md.push_str("> Source: `unresolved-refs.md`\n\n");
        let lines: Vec<&str> = report.lines().take(40).collect();
        for line in &lines {
            md.push_str(line);
            md.push('\n');
        }
        if report.lines().count() > 40 {
            md.push_str("\n_(truncated — see `unresolved-refs.md` for full report)_\n");
        }
        md.push('\n');
    }

    md
}

pub fn render_dev_community(
    graph: &WikiGraph,
    community: &Node,
    slug_map: &BTreeMap<String, String>,
    llm: Option<&CommunityLlmSummary>,
) -> String {
    let comm_id = community.id.as_str();
    let slug = slug_map.get(comm_id).map(|s| s.as_str()).unwrap_or(comm_id);

    let mut md = String::new();
    md.push_str(&format!(
        "---\nid: dev/{}\ntitle: {}\n---\n\n",
        slug, community.name
    ));
    md.push_str(&format!("# {} — Technical Reference\n\n", community.name));

    if let Some(summary) = llm {
        if !summary.dev.is_empty() {
            md.push_str("## Summary\n\n");
            md.push_str(&summary.dev);
            md.push_str("\n\n");
        }
    }

    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    let classes: Vec<&Node> = member_list
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Enum
                    | NodeKind::Record
                    | NodeKind::Annotation
            )
        })
        .collect();

    if !classes.is_empty() {
        md.push_str("## Classes\n\n");
        md.push_str("| Class | Stereotype | Tests |\n");
        md.push_str("|---|---|---|\n");

        for cls in &classes {
            let stereotype = node_stereotype(cls).unwrap_or("—");
            let test_names: Vec<&str> = graph
                .tests_in
                .get(cls.id.as_str())
                .iter()
                .flat_map(|ids| ids.iter())
                .filter_map(|id| graph.nodes_by_id.get(id).map(|n| n.name.as_str()))
                .collect();
            let test_display = if test_names.is_empty() {
                "—".to_string()
            } else {
                test_names.join(", ")
            };
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                cls.name, stereotype, test_display
            ));
        }
        md.push('\n');
    }

    if let Some(routes) = graph.community_routes.get(comm_id) {
        if !routes.is_empty() {
            md.push_str("## Routes\n\n");
            md.push_str("| Method | Path | Handler |\n");
            md.push_str("|---|---|---|\n");
            for (handler, route) in routes {
                md.push_str(&format!(
                    "| `{}` | `{}` | `{}` |\n",
                    route_http_method(route),
                    route_path(route),
                    handler.name,
                ));
            }
            md.push('\n');
        }
    }

    let mut ext_call_names: Vec<String> = Vec::new();
    for m in member_list {
        if let Some(ext_ids) = graph.external_calls.get(m.id.as_str()) {
            for eid in ext_ids {
                if let Some(ext_node) = graph.nodes_by_id.get(eid) {
                    if !ext_call_names.contains(&ext_node.name) {
                        ext_call_names.push(ext_node.name.clone());
                    }
                }
            }
        }
    }
    if !ext_call_names.is_empty() {
        md.push_str("## External Calls\n\n");
        for name in &ext_call_names {
            md.push_str(&format!("- `{}`\n", name));
        }
        md.push('\n');
    }

    if let Some(test_ids) = graph.community_tests.get(comm_id) {
        if !test_ids.is_empty() {
            md.push_str("## Test Coverage\n\n");
            for tid in test_ids {
                if let Some(test_node) = graph.nodes_by_id.get(tid) {
                    md.push_str(&format!("- `{}`\n", test_node.name));
                }
            }
            md.push('\n');
        }
    }

    let mut files: Vec<&str> = member_list
        .iter()
        .filter(|n| !n.file.is_empty())
        .map(|n| n.file.as_str())
        .collect();
    files.sort_unstable();
    files.dedup();

    if !files.is_empty() {
        md.push_str("## Important Files\n\n");
        for f in files.iter().take(10) {
            md.push_str(&format!("- `{}`\n", f));
        }
        if files.len() > 10 {
            md.push_str(&format!("- _…and {} more_\n", files.len() - 10));
        }
        md.push('\n');
    }

    md
}

pub fn render_dev_community_json(graph: &WikiGraph, community: &Node) -> serde_json::Value {
    let comm_id = community.id.as_str();
    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    let classes: Vec<&Node> = member_list
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Enum
                    | NodeKind::Record
                    | NodeKind::Annotation
            )
        })
        .collect();

    let nodes: Vec<serde_json::Value> = classes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id.as_str(),
                "label": n.name.as_str(),
                "kind": n.kind.label(),
                "stereotype": node_stereotype(n).unwrap_or(""),
            })
        })
        .collect();

    let class_ids: std::collections::HashSet<String> =
        classes.iter().map(|n| n.id.as_str().to_string()).collect();

    let links: Vec<serde_json::Value> = classes
        .iter()
        .flat_map(|cls| {
            let src_id = cls.id.as_str().to_string();
            let empty: Vec<String> = Vec::new();
            let dsts = graph.calls_out.get(&src_id).unwrap_or(&empty);
            dsts.iter()
                .filter(|d| class_ids.contains(*d))
                .map(move |dst| {
                    serde_json::json!({
                        "source": &src_id,
                        "target": dst,
                        "label": "CALLS",
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    serde_json::json!({
        "format": "d3-force",
        "community_id": comm_id,
        "nodes": nodes,
        "links": links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

    fn make_node(id: &str, kind: NodeKind, name: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: "Test.java".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn simple_dev_graph() -> WikiGraph {
        let cls = Node {
            id: NodeId::new("Class:com.example.OrderService".to_string()),
            kind: NodeKind::Class,
            name: "OrderService".to_string(),
            qualified_name: Some("com.example.OrderService".to_string()),
            file: "OrderService.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({"stereotype": "service"})),
        };
        let test_cls = Node {
            id: NodeId::new("Class:com.example.OrderServiceTest".to_string()),
            kind: NodeKind::Class,
            name: "OrderServiceTest".to_string(),
            qualified_name: Some("com.example.OrderServiceTest".to_string()),
            file: "OrderServiceTest.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({"stereotype": "test"})),
        };
        let comm = make_node("Community:0", NodeKind::Community, "order-service");
        let nodes = [cls.clone(), test_cls.clone()];
        let edges = [Edge {
            src: test_cls.id.clone(),
            dst: cls.id.clone(),
            kind: EdgeKind::Tests,
            confidence: 1.0,
            reason: String::new(),
        }];
        let comm_edges = [
            Edge {
                src: cls.id.clone(),
                dst: NodeId::new("Community:0".to_string()),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: test_cls.id.clone(),
                dst: NodeId::new("Community:0".to_string()),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        WikiGraph::build(&nodes, &edges, &[comm], &comm_edges)
    }

    fn slug_map() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("Community:0".to_string(), "order-service".to_string());
        m
    }

    #[test]
    fn render_dev_community_shows_classes() {
        let g = simple_dev_graph();
        let comm = g.community_nodes[0].clone();
        let md = render_dev_community(&g, &comm, &slug_map(), None);
        assert!(md.contains("---\nid: dev/order-service"), "has frontmatter");
        assert!(md.contains("OrderService"), "has class name");
        assert!(md.contains("service"), "has stereotype");
    }

    #[test]
    fn render_dev_community_writes_d3_sidecar_shape() {
        let g = simple_dev_graph();
        let comm = g.community_nodes[0].clone();
        let val = render_dev_community_json(&g, &comm);
        assert_eq!(val["format"], "d3-force");
        assert!(val["nodes"].is_array());
        assert!(val["links"].is_array());
    }

    #[test]
    fn render_dev_community_inserts_technical_summary_when_present() {
        let g = simple_dev_graph();
        let comm = g.community_nodes[0].clone();
        let llm = CommunityLlmSummary {
            po: String::new(),
            ba: String::new(),
            dev: "Service-repository pattern with 8 methods.".to_string(),
        };
        let md = render_dev_community(&g, &comm, &slug_map(), Some(&llm));
        assert!(md.contains("## Summary"), "has summary section");
        assert!(
            md.contains("Service-repository pattern"),
            "has llm text"
        );
    }
}
