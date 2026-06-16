use std::collections::BTreeMap;
use cih_core::Node;
use crate::graph::{route_http_method, route_path, WikiGraph};
use crate::CommunityLlmSummary;

pub fn render_po_index(
    graph: &WikiGraph,
    slug_map: &BTreeMap<String, String>,
    llm_enriched: bool,
) -> String {
    let mut md = String::new();
    md.push_str("---\ntitle: System Overview\n---\n\n");
    md.push_str("# System Overview\n\n");

    if llm_enriched {
        md.push_str("> AI enrichment active\n\n");
    }

    md.push_str(&format!(
        "**Communities:** {} · **Routes:** {} · **Processes:** {}\n\n",
        graph.community_nodes.len(),
        graph.routes.len(),
        graph.process_nodes.len(),
    ));

    if graph.community_nodes.is_empty() {
        md.push_str("No communities found. Run `discover` first.\n");
        return md;
    }

    md.push_str("## Business Capabilities\n\n");
    md.push_str("| Module | Routes | Processes | Classes |\n");
    md.push_str("|---|---|---|---|\n");

    for comm in &graph.community_nodes {
        let comm_id = comm.id.as_str();
        let slug = slug_map.get(comm_id).map(|s| s.as_str()).unwrap_or(comm_id);
        let route_count = graph
            .community_routes
            .get(comm_id)
            .map(|r| r.len())
            .unwrap_or(0);
        let process_count = processes_for_community(graph, comm_id).len();
        let class_count = graph
            .community_class_counts
            .get(comm_id)
            .copied()
            .unwrap_or(0);
        md.push_str(&format!(
            "| [{}](po/{}.md) | {} | {} | {} |\n",
            comm.name, slug, route_count, process_count, class_count
        ));
    }

    md
}

pub fn render_po_community(
    graph: &WikiGraph,
    community: &Node,
    slug_map: &BTreeMap<String, String>,
    llm: Option<&CommunityLlmSummary>,
) -> String {
    let comm_id = community.id.as_str();
    let slug = slug_map.get(comm_id).map(|s| s.as_str()).unwrap_or(comm_id);

    let mut md = String::new();
    md.push_str(&format!(
        "---\ntitle: {}\n---\n\n",
        community.name
    ));
    md.push_str(&format!("# {}\n\n", community.name));

    if let Some(summary) = llm {
        if !summary.po.is_empty() {
            md.push_str("## Overview\n\n");
            md.push_str(&summary.po);
            md.push_str("\n\n");
        }
    }

    let class_count = graph
        .community_class_counts
        .get(comm_id)
        .copied()
        .unwrap_or(0);
    let method_count = graph
        .community_method_counts
        .get(comm_id)
        .copied()
        .unwrap_or(0);
    let route_count = graph
        .community_routes
        .get(comm_id)
        .map(|r| r.len())
        .unwrap_or(0);
    let test_count = graph
        .community_tests
        .get(comm_id)
        .map(|t| t.len())
        .unwrap_or(0);

    md.push_str(&format!(
        "**Symbols:** {} · **Routes:** {} · **Tests:** {}\n\n",
        class_count + method_count,
        route_count,
        test_count
    ));

    if let Some(routes) = graph.community_routes.get(comm_id) {
        if !routes.is_empty() {
            md.push_str("## Business Capabilities (Routes)\n\n");
            md.push_str("| Method | Path |\n");
            md.push_str("|---|---|\n");
            for (_, route) in routes {
                md.push_str(&format!(
                    "| `{}` | `{}` |\n",
                    route_http_method(route),
                    route_path(route),
                ));
            }
            md.push('\n');
        }
    }

    if let Some(tables) = graph.community_db_tables.get(comm_id) {
        if !tables.is_empty() {
            md.push_str("## Core Tables\n\n");
            md.push_str("| Table | Access |\n");
            md.push_str("|---|---|\n");
            for t in tables {
                let access = match (t.reads, t.writes) {
                    (true, true) => "Read + Write",
                    (true, false) => "Read",
                    (false, true) => "Write",
                    _ => "—",
                };
                md.push_str(&format!("| `{}` | {} |\n", t.table_name, access));
            }
            md.push('\n');
        }
    }

    let procs = processes_for_community(graph, comm_id);
    if !procs.is_empty() {
        md.push_str("## Business Processes\n\n");
        for proc_id in procs {
            if let Some(proc_node) = graph.nodes_by_id.get(&proc_id) {
                md.push_str(&format!("### {}\n\n", proc_node.name));
                if let Some(steps) = graph.process_steps.get(&proc_id) {
                    let chain: Vec<&str> =
                        steps.iter().map(|s| s.symbol.name.as_str()).collect();
                    md.push_str(&chain.join(" → "));
                    md.push_str("\n\n");
                }
            }
        }
    }

    if test_count > 0 {
        md.push_str("## Test Coverage\n\n");
        md.push_str(&format!(
            "{} test class(es) covering this module.\n\n",
            test_count
        ));
    }

    md
}

fn processes_for_community(graph: &WikiGraph, community_id: &str) -> Vec<String> {
    let mut result = Vec::new();
    for (proc_id, steps) in &graph.process_steps {
        if let Some(first) = steps.first() {
            let sym_id = first.symbol.id.as_str().to_string();
            if graph
                .community_by_member
                .get(&sym_id)
                .map(|c| c.as_str())
                == Some(community_id)
            {
                result.push(proc_id.clone());
            }
        }
    }
    result.sort();
    result
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
            file: String::new(),
            range: Range::default(),
            props: None,
        }
    }

    fn simple_graph() -> WikiGraph {
        let sym = make_node(
            "Method:com.example.OrderController#list/0",
            NodeKind::Method,
            "list",
        );
        let comm = make_node("Community:0", NodeKind::Community, "order-service");
        let route = Node {
            id: NodeId::new("Route:GET /api/orders".to_string()),
            kind: NodeKind::Route,
            name: "GET /api/orders".to_string(),
            qualified_name: None,
            file: "OrderController.java".to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "httpMethod": "GET", "path": "/api/orders", "decorator": "GetMapping"
            })),
        };
        let nodes = [sym.clone(), route.clone()];
        let edges = [Edge {
            src: sym.id.clone(),
            dst: route.id.clone(),
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: String::new(),
        }];
        let comm_edges = [Edge {
            src: sym.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }];
        WikiGraph::build(&nodes, &edges, &[comm], &comm_edges)
    }

    fn slug_map() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("Community:0".to_string(), "order-service".to_string());
        m
    }

    #[test]
    fn render_po_index_lists_communities() {
        let g = simple_graph();
        let md = render_po_index(&g, &slug_map(), false);
        assert!(md.contains("---\ntitle: System Overview"), "has frontmatter");
        assert!(md.contains("order-service"), "has community name");
        assert!(md.contains("Business Capabilities"), "has section header");
    }

    #[test]
    fn render_po_community_shows_routes_and_processes() {
        let g = simple_graph();
        let comm = g.community_nodes[0].clone();
        let md = render_po_community(&g, &comm, &slug_map(), None);
        assert!(md.contains("---\ntitle: order-service"), "has frontmatter");
        assert!(md.contains("/api/orders"), "has route path");
    }

    #[test]
    fn render_po_community_inserts_llm_summary_when_present() {
        let g = simple_graph();
        let comm = g.community_nodes[0].clone();
        let llm = CommunityLlmSummary {
            po: "Handles order management.".to_string(),
            ba: String::new(),
            dev: String::new(),
        };
        let md = render_po_community(&g, &comm, &slug_map(), Some(&llm));
        assert!(md.contains("## Overview"), "has overview section");
        assert!(md.contains("Handles order management"), "has summary text");
    }

    #[test]
    fn render_po_community_shows_core_tables_when_db_access_present() {
        use cih_core::{EdgeKind, NodeId};
        let method = make_node(
            "Method:com.example.OrderService#find/0",
            NodeKind::Method,
            "find",
        );
        let dbq = make_node("DbQuery:com.example.OrderService#SQL", NodeKind::DbQuery, "SQL");
        let tbl = make_node("DbTable:ORDERS", NodeKind::DbTable, "ORDERS");
        let comm = make_node("Community:0", NodeKind::Community, "order-service");
        let nodes = [method.clone(), dbq.clone(), tbl.clone()];
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
                kind: EdgeKind::ReadsTable,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let comm_edges = [Edge {
            src: method.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }];
        let g = WikiGraph::build(&nodes, &edges, &[comm], &comm_edges);
        let comm_node = g.community_nodes[0].clone();
        let md = render_po_community(&g, &comm_node, &slug_map(), None);
        assert!(md.contains("## Core Tables"), "has core tables section");
        assert!(md.contains("ORDERS"), "has table name");
        assert!(md.contains("Read"), "has access type");
    }

    #[test]
    fn render_po_community_omits_core_tables_when_none() {
        let g = simple_graph();
        let comm = g.community_nodes[0].clone();
        let md = render_po_community(&g, &comm, &slug_map(), None);
        assert!(!md.contains("## Core Tables"), "no core tables section when empty");
    }

    #[test]
    fn render_po_community_omits_overview_section_when_no_summary() {
        let g = simple_graph();
        let comm = g.community_nodes[0].clone();
        let md = render_po_community(&g, &comm, &slug_map(), None);
        assert!(!md.contains("## Overview"), "overview section absent when no llm");
    }
}
