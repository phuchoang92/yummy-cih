use super::*;
use cih_core::{NodeId, NodeKind, Range};

fn node(id: &str, kind: NodeKind, name: &str) -> Node {
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

fn step_edge(sym_id: &str, proc_id: &str, step_n: usize) -> Edge {
    Edge {
        src: NodeId::new(sym_id.to_string()),
        dst: NodeId::new(proc_id.to_string()),
        kind: EdgeKind::StepInProcess,
        confidence: 1.0,
        reason: format!("step:{}", step_n),
            props: None,
    }
}

#[test]
fn wiki_graph_indexes_community_members() {
    let sym = node("Method:com.example.Foo#bar/0", NodeKind::Method, "bar");
    let comm = node("Community:0", NodeKind::Community, "order-service");
    let comm_edges = [Edge {
        src: NodeId::new("Method:com.example.Foo#bar/0".to_string()),
        dst: NodeId::new("Community:0".to_string()),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
            props: None,
    }];
    let g = WikiGraph::build(&[sym], &[], &[comm], &comm_edges);
    assert_eq!(g.community_nodes.len(), 1);
    assert_eq!(g.members_by_community["Community:0"].len(), 1);
    assert_eq!(
        g.community_by_member["Method:com.example.Foo#bar/0"],
        "Community:0"
    );
}

#[test]
fn wiki_graph_indexes_routes() {
    let handler = node("Method:com.example.Ctrl#list/0", NodeKind::Method, "list");
    let route = Node {
        id: NodeId::new("Route:GET /api/orders".to_string()),
        kind: NodeKind::Route,
        name: "GET /api/orders".to_string(),
        qualified_name: None,
        file: "Ctrl.java".to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "httpMethod": "GET",
            "path": "/api/orders",
            "decorator": "GetMapping",
        })),
    };
    let e = Edge {
        src: handler.id.clone(),
        dst: route.id.clone(),
        kind: EdgeKind::HandlesRoute,
        confidence: 1.0,
        reason: String::new(),
            props: None,
    };
    let g = WikiGraph::build(&[handler, route], &[e], &[], &[]);
    assert_eq!(g.routes.len(), 1);
    assert_eq!(route_path(&g.routes[0].1), "/api/orders");
    assert_eq!(route_http_method(&g.routes[0].1), "GET");
}

#[test]
fn wiki_graph_indexes_db_table_access() {
    let method = node("Method:com.example.Foo#find/0", NodeKind::Method, "find");
    let dbq = node(
        "DbQuery:com.example.Foo#SQL_FIND",
        NodeKind::DbQuery,
        "SQL_FIND",
    );
    let tbl_orders = node("DbTable:ORDERS", NodeKind::DbTable, "ORDERS");
    let tbl_status = node("DbTable:ORDER_STATUS", NodeKind::DbTable, "ORDER_STATUS");
    let comm = node("Community:0", NodeKind::Community, "order-svc");

    let nodes = [
        method.clone(),
        dbq.clone(),
        tbl_orders.clone(),
        tbl_status.clone(),
    ];
    let edges = [
        Edge {
            src: method.id.clone(),
            dst: dbq.id.clone(),
            kind: EdgeKind::ExecutesQuery,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: dbq.id.clone(),
            dst: tbl_orders.id.clone(),
            kind: EdgeKind::ReadsTable,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: dbq.id.clone(),
            dst: tbl_status.id.clone(),
            kind: EdgeKind::WritesTable,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
    ];
    let comm_edges = [Edge {
        src: method.id.clone(),
        dst: comm.id.clone(),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
            props: None,
    }];

    let g = WikiGraph::build(&nodes, &edges, &[comm], &comm_edges);

    let tables = g.community_db_tables.get("Community:0").unwrap();
    assert_eq!(tables.len(), 2);
    // "ORDERS" < "ORDER_STATUS" because 'S' (83) < '_' (95)
    assert_eq!(tables[0].table_name, "ORDERS");
    assert!(tables[0].reads);
    assert!(!tables[0].writes);
    assert_eq!(tables[1].table_name, "ORDER_STATUS");
    assert!(!tables[1].reads);
    assert!(tables[1].writes);
}

#[test]
fn wiki_graph_orders_process_steps_from_edge_reasons() {
    let proc = node("Process:order-create", NodeKind::Process, "order-create");
    let sym1 = node("Method:A#step1/0", NodeKind::Method, "step1");
    let sym2 = node("Method:B#step2/0", NodeKind::Method, "step2");
    let sym3 = node("Method:C#step3/0", NodeKind::Method, "step3");
    let all_nodes = [sym1, sym2, sym3];
    let comm_edges = [
        step_edge("Method:C#step3/0", "Process:order-create", 2),
        step_edge("Method:A#step1/0", "Process:order-create", 0),
        step_edge("Method:B#step2/0", "Process:order-create", 1),
    ];
    let g = WikiGraph::build(&all_nodes, &[], &[proc], &comm_edges);
    let steps = &g.process_steps["Process:order-create"];
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].step_number, 0);
    assert_eq!(steps[1].step_number, 1);
    assert_eq!(steps[2].step_number, 2);
    assert_eq!(steps[0].symbol.name, "step1");
    assert_eq!(steps[1].symbol.name, "step2");
    assert_eq!(steps[2].symbol.name, "step3");
}

// Feature-path tests live in cih-grouping/src/strategies/package.rs
