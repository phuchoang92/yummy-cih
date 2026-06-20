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
    assert!(
        md.contains("---\ntitle: System Overview"),
        "has frontmatter"
    );
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
    let dbq = make_node(
        "DbQuery:com.example.OrderService#SQL",
        NodeKind::DbQuery,
        "SQL",
    );
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
    assert!(
        !md.contains("## Core Tables"),
        "no core tables section when empty"
    );
}

#[test]
fn render_po_community_omits_overview_section_when_no_summary() {
    let g = simple_graph();
    let comm = g.community_nodes[0].clone();
    let md = render_po_community(&g, &comm, &slug_map(), None);
    assert!(
        !md.contains("## Overview"),
        "overview section absent when no llm"
    );
}
