use crate::graph::{node_stereotype, route_http_method, route_path, WikiGraph};
use crate::mermaid;
use crate::{CommunityLlmFull, CommunityLlmSummary};
use cih_core::{Node, NodeKind, RepoMap};
use std::collections::BTreeMap;

fn method_signature(node: &Node) -> String {
    let params = node
        .props
        .as_ref()
        .and_then(|p| p.get("paramTypes"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!("{}({})", node.name, params)
}

fn callee_display(node: &Node) -> String {
    if let Some(qn) = &node.qualified_name {
        if let Some(hash_pos) = qn.find('#') {
            let class_part = &qn[..hash_pos];
            let simple = class_part.rsplit('.').next().unwrap_or(class_part);
            return format!("{}.{}", simple, node.name);
        }
    }
    node.name.clone()
}

fn format_return_type(node: &Node) -> &str {
    node.props
        .as_ref()
        .and_then(|p| p.get("returnType"))
        .and_then(|v| v.as_str())
        .unwrap_or("void")
}

pub fn render_dev_index(
    graph: &WikiGraph,
    repo_map: Option<&RepoMap>,
    unresolved_report: Option<&str>,
) -> String {
    let mut md = String::new();
    md.push_str("---\ntitle: Technical Overview\nrole: dev\n---\n\n");
    md.push_str("<div class=\"role-banner role-dev\"><span class=\"role-dot\"></span>Developer<span class=\"role-desc\">Technical structure, calls &amp; tests</span></div>\n\n");
    md.push_str("# Technical Overview\n\n");

    md.push_str("## Community Summary\n\n");
    md.push_str("| Module | Classes | Methods | Routes | Tests |\n");
    md.push_str("|---|---|---|---|---|\n");

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let classes = graph
            .community_class_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        let methods = graph
            .community_method_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        let routes = graph
            .community_routes
            .get(comm_id)
            .map(|r| r.len())
            .unwrap_or(0);
        let tests = graph
            .community_tests
            .get(comm_id)
            .map(|t| t.len())
            .unwrap_or(0);
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

/// `page_path` is the full path without "pages/" prefix, e.g. `"payment/dev/payment-controller"`.
pub fn render_dev_community(
    graph: &WikiGraph,
    community: &Node,
    page_path: &str,
    llm: Option<&CommunityLlmSummary>,
    llm_full: Option<&CommunityLlmFull>,
) -> String {
    let comm_id = community.id.as_str();
    // Derive a unique title from the last path segment (e.g. "warehouse-service-2" → "Warehouse Service 2")
    let page_title = page_path
        .split('/')
        .last()
        .map(|s| {
            s.split('-')
                .map(|word| {
                    let mut c = word.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| community.name.clone());

    let mut md = String::new();
    md.push_str(&format!("---\ntitle: {}\nrole: dev\n---\n\n", page_title));
    md.push_str("<div class=\"role-banner role-dev\"><span class=\"role-dot\"></span>Developer<span class=\"role-desc\">Technical structure, calls &amp; tests</span></div>\n\n");
    md.push_str(&format!("# {} — Technical Reference\n\n", page_title));

    if let Some(full) = llm_full {
        if !full.dev_responsibility.is_empty() {
            md.push_str("## Responsibility\n\n");
            md.push_str(&full.dev_responsibility);
            md.push_str("\n\n");
        }
        if !full.dev_key_classes.is_empty() {
            md.push_str("## Key Classes\n\n");
            md.push_str(&full.dev_key_classes);
            md.push_str("\n\n");
        }
        if !full.dev_entry_points.is_empty() {
            md.push_str("## Entry Points\n\n");
            md.push_str(&full.dev_entry_points);
            md.push_str("\n\n");
        }
    } else if let Some(summary) = llm {
        if !summary.dev.is_empty() {
            md.push_str("## Summary\n\n");
            md.push_str(&summary.dev);
            md.push_str("\n\n");
        }
    }

    // Class-level call diagram: shows which classes this community calls (and is called by).
    // Operates on class-to-class edges rather than community-to-community, so it correctly
    // shows controller→service relationships even when Louvain co-locates them.
    if let Some(diagram) = mermaid::class_call_diagram(graph, comm_id) {
        md.push_str("## Class Interactions\n\n");
        md.push_str("```mermaid\n");
        md.push_str(&diagram);
        md.push_str("```\n\n");
    }

    let empty_members: Vec<Node> = Vec::new();
    let member_list = graph
        .members_by_community
        .get(comm_id)
        .unwrap_or(&empty_members);

    // Communities group methods (not classes); derive the parent class from each method's ID.
    // Method id format: "Method:fqcn#name/arity" → class id: "Class:fqcn"
    let mut class_to_methods: BTreeMap<String, Vec<&Node>> = BTreeMap::new();
    for m in member_list {
        if !matches!(
            m.kind,
            NodeKind::Method | NodeKind::Function | NodeKind::Constructor
        ) {
            continue;
        }
        let cls_id =
            m.id.as_str()
                .split_once('#')
                .map(|(prefix, _)| {
                    let fqcn = prefix
                        .trim_start_matches("Method:")
                        .trim_start_matches("Constructor:");
                    format!("Class:{}", fqcn)
                })
                .unwrap_or_default();
        if !cls_id.is_empty() {
            class_to_methods.entry(cls_id).or_default().push(m);
        }
    }

    if !class_to_methods.is_empty() {
        md.push_str("## Classes\n\n");

        for (cls_id, methods) in &class_to_methods {
            let cls = graph.nodes_by_id.get(cls_id);
            let cls_name = cls.map(|n| n.name.as_str()).unwrap_or_else(|| {
                cls_id
                    .trim_start_matches("Class:")
                    .rsplit('.')
                    .next()
                    .unwrap_or(cls_id)
            });
            let stereotype = cls.and_then(node_stereotype).unwrap_or("—");

            let test_names: Vec<&str> = cls
                .map(|c| {
                    graph
                        .tests_in
                        .get(c.id.as_str())
                        .into_iter()
                        .flatten()
                        .filter_map(|id| graph.nodes_by_id.get(id).map(|n| n.name.as_str()))
                        .collect()
                })
                .unwrap_or_default();

            md.push_str(&format!("### `{}` · {}\n\n", cls_name, stereotype));

            let file = cls
                .map(|c| c.file.as_str())
                .unwrap_or_else(|| methods.first().map(|m| m.file.as_str()).unwrap_or(""));
            if !file.is_empty() {
                let line = cls.map(|c| c.range.start_line).unwrap_or(0);
                if line > 0 {
                    md.push_str(&format!("`{}` :{}\n\n", file, line));
                } else {
                    md.push_str(&format!("`{}`\n\n", file));
                }
            }

            if !test_names.is_empty() {
                md.push_str(&format!("Tests: {}\n\n", test_names.join(", ")));
            }

            let visible: Vec<&&Node> = methods
                .iter()
                .filter(|m| !matches!(m.kind, NodeKind::Constructor))
                .collect();

            if !visible.is_empty() {
                md.push_str("| Method | Returns | Line | Calls |\n");
                md.push_str("|---|---|---|---|\n");
                for method in visible.iter().take(20) {
                    let sig = method_signature(method);
                    let ret = format_return_type(method);
                    let line = if method.range.start_line > 0 {
                        format!(":{}", method.range.start_line)
                    } else {
                        String::new()
                    };
                    let empty_calls: Vec<String> = Vec::new();
                    let calls_display = graph
                        .calls_out
                        .get(method.id.as_str())
                        .unwrap_or(&empty_calls)
                        .iter()
                        .take(3)
                        .filter_map(|cid| graph.nodes_by_id.get(cid))
                        .map(callee_display)
                        .collect::<Vec<_>>()
                        .join(", ");
                    md.push_str(&format!(
                        "| `{}` | `{}` | {} | {} |\n",
                        sig, ret, line, calls_display
                    ));
                }
                if visible.len() > 20 {
                    md.push_str(&format!("\n_…and {} more methods_\n", visible.len() - 20));
                }
                md.push('\n');
            }
        }
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

    if let Some(tables) = graph.community_db_tables.get(comm_id) {
        if !tables.is_empty() {
            md.push_str("## DB Access\n\n");
            md.push_str("| Table | Read | Write |\n");
            md.push_str("|---|---|---|\n");
            for t in tables {
                md.push_str(&format!(
                    "| `{}` | {} | {} |\n",
                    t.table_name,
                    if t.reads { "✓" } else { "" },
                    if t.writes { "✓" } else { "" },
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
        let method = Node {
            id: NodeId::new("Method:com.example.OrderService#find/0".to_string()),
            kind: NodeKind::Method,
            name: "find".to_string(),
            qualified_name: Some("com.example.OrderService#find/0".to_string()),
            file: "OrderService.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({"returnType": "Order"})),
        };
        let comm = make_node("Community:0", NodeKind::Community, "order-service");
        let nodes = [cls.clone(), test_cls.clone(), method.clone()];
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
            Edge {
                src: method.id.clone(),
                dst: NodeId::new("Community:0".to_string()),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        WikiGraph::build(&nodes, &edges, &[comm], &comm_edges)
    }

    #[test]
    fn render_dev_community_shows_classes() {
        let g = simple_dev_graph();
        let comm = g.community_nodes[0].clone();
        let md = render_dev_community(&g, &comm, "shared/dev/order-service", None, None);
        assert!(md.contains("---\ntitle: Order Service"), "has frontmatter");
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
    fn render_dev_community_shows_db_access_when_present() {
        use cih_core::{EdgeKind, NodeId};
        let cls = Node {
            id: NodeId::new("Class:com.example.OrderService".to_string()),
            kind: NodeKind::Class,
            name: "OrderService".to_string(),
            qualified_name: Some("com.example.OrderService".to_string()),
            file: "OrderService.java".to_string(),
            range: cih_core::Range::default(),
            props: Some(serde_json::json!({"stereotype": "service"})),
        };
        let method = make_node(
            "Method:com.example.OrderService#save/0",
            NodeKind::Method,
            "save",
        );
        let dbq = make_node(
            "DbQuery:com.example.OrderService#SQL_SAVE",
            NodeKind::DbQuery,
            "SQL_SAVE",
        );
        let tbl = make_node("DbTable:ORDERS", NodeKind::DbTable, "ORDERS");
        let comm = make_node("Community:0", NodeKind::Community, "order-service");
        let nodes = [cls.clone(), method.clone(), dbq.clone(), tbl.clone()];
        let edges = [
            Edge {
                src: method.id.clone(),
                dst: dbq.id.clone(),
                kind: EdgeKind::ExecutesQuery,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: dbq.id.clone(),
                dst: tbl.id.clone(),
                kind: EdgeKind::WritesTable,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let comm_edges = [
            Edge {
                src: cls.id.clone(),
                dst: NodeId::new("Community:0".to_string()),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: method.id.clone(),
                dst: NodeId::new("Community:0".to_string()),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let g = WikiGraph::build(&nodes, &edges, &[comm], &comm_edges);
        let comm_node = g.community_nodes[0].clone();
        let md = render_dev_community(&g, &comm_node, "shared/dev/order-service", None, None);
        assert!(md.contains("## DB Access"), "has db access section");
        assert!(md.contains("ORDERS"), "has table name");
        assert!(md.contains("✓"), "has check mark");
    }

    #[test]
    fn render_dev_community_omits_db_access_when_none() {
        let g = simple_dev_graph();
        let comm = g.community_nodes[0].clone();
        let md = render_dev_community(&g, &comm, "shared/dev/order-service", None, None);
        assert!(
            !md.contains("## DB Access"),
            "no db access section when no tables"
        );
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
        let md = render_dev_community(&g, &comm, "shared/dev/order-service", Some(&llm), None);
        assert!(md.contains("## Summary"), "has summary section");
        assert!(md.contains("Service-repository pattern"), "has llm text");
    }
}
