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
            props: None,
    }];
    let comm_edges = [
        Edge {
            src: cls.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: test_cls.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: method.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
    ];
    WikiGraph::build(&nodes, &edges, &[comm], &comm_edges)
}

#[test]
fn render_dev_community_shows_classes() {
    let g = simple_dev_graph();
    let comm = g.community_nodes[0].clone();
    let md = render_dev_community(&g, &comm, "shared/dev/order-service", None, None, &HashMap::new());
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
                props: None,
        },
        Edge {
            src: dbq.id.clone(),
            dst: tbl.id.clone(),
            kind: EdgeKind::WritesTable,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
    ];
    let comm_edges = [
        Edge {
            src: cls.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
        Edge {
            src: method.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        },
    ];
    let g = WikiGraph::build(&nodes, &edges, &[comm], &comm_edges);
    let comm_node = g.community_nodes[0].clone();
    let md = render_dev_community(&g, &comm_node, "shared/dev/order-service", None, None, &HashMap::new());
    assert!(md.contains("## DB Access"), "has db access section");
    assert!(md.contains("ORDERS"), "has table name");
    assert!(md.contains("✓"), "has check mark");
}

#[test]
fn render_dev_community_omits_db_access_when_none() {
    let g = simple_dev_graph();
    let comm = g.community_nodes[0].clone();
    let md = render_dev_community(&g, &comm, "shared/dev/order-service", None, None, &HashMap::new());
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
    let md = render_dev_community(&g, &comm, "shared/dev/order-service", Some(&llm), None, &HashMap::new());
    assert!(md.contains("## Summary"), "has summary section");
    assert!(md.contains("Service-repository pattern"), "has llm text");
}
