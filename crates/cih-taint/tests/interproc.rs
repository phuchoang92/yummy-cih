use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_taint::{default_rules, find_taint_paths, SinkCategory};

fn method_node(id: &str) -> Node {
    Node {
        id: NodeId::new(id),
        kind: NodeKind::Method,
        name: id.split('#').next_back().unwrap_or(id).to_string(),
        qualified_name: Some(id.to_string()),
        file: "Test.java".to_string(),
        range: Range::default(),
        props: None,
    }
}

fn db_query_node(id: &str, dynamic: bool) -> Node {
    Node {
        id: NodeId::new(id),
        kind: NodeKind::DbQuery,
        name: id.to_string(),
        qualified_name: None,
        file: "Test.java".to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({ "dynamic": dynamic })),
    }
}

fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
    Edge::new(NodeId::new(src), NodeId::new(dst), kind, 1.0, String::new())
}

#[test]
fn direct_source_to_sql_sink_via_executes_query() {
    let nodes = vec![
        method_node("Method:com.example.OrderController#create/1"),
        method_node("Method:com.example.OrderDao#save/1"),
        db_query_node("DbQuery:OrderDao:10:5", true),
    ];
    let edges = vec![
        edge(
            "Method:com.example.OrderController#create/1",
            "Route:/api/orders",
            EdgeKind::HandlesRoute,
        ),
        edge(
            "Method:com.example.OrderController#create/1",
            "Method:com.example.OrderDao#save/1",
            EdgeKind::Calls,
        ),
        edge(
            "Method:com.example.OrderDao#save/1",
            "DbQuery:OrderDao:10:5",
            EdgeKind::ExecutesQuery,
        ),
    ];

    let rules = default_rules();
    let paths = find_taint_paths(&nodes, &edges, &rules);

    assert_eq!(paths.len(), 1);
    assert_eq!(
        paths[0].source.as_str(),
        "Method:com.example.OrderController#create/1"
    );
    assert_eq!(
        paths[0].sink_method.as_str(),
        "Method:com.example.OrderDao#save/1"
    );
    assert_eq!(paths[0].category, SinkCategory::Sql);
    assert_eq!(paths[0].hops.len(), 2);
}

#[test]
fn static_sql_not_a_sink() {
    let nodes = vec![
        method_node("Method:com.example.OrderController#create/1"),
        method_node("Method:com.example.OrderDao#save/1"),
        db_query_node("DbQuery:OrderDao:10:5", false),
    ];
    let edges = vec![
        edge(
            "Method:com.example.OrderController#create/1",
            "Route:/api/orders",
            EdgeKind::HandlesRoute,
        ),
        edge(
            "Method:com.example.OrderController#create/1",
            "Method:com.example.OrderDao#save/1",
            EdgeKind::Calls,
        ),
        edge(
            "Method:com.example.OrderDao#save/1",
            "DbQuery:OrderDao:10:5",
            EdgeKind::ExecutesQuery,
        ),
    ];

    let rules = default_rules();
    let paths = find_taint_paths(&nodes, &edges, &rules);
    assert!(paths.is_empty(), "static SQL should not be a taint sink");
}

#[test]
fn multi_hop_exec_sink() {
    let nodes = vec![
        method_node("Method:com.example.CommandController#run/1"),
        method_node("Method:com.example.CommandService#execute/1"),
        method_node("Method:java.lang.Runtime#exec/1"),
    ];
    let edges = vec![
        edge(
            "Method:com.example.CommandController#run/1",
            "Route:/api/run",
            EdgeKind::HandlesRoute,
        ),
        edge(
            "Method:com.example.CommandController#run/1",
            "Method:com.example.CommandService#execute/1",
            EdgeKind::Calls,
        ),
        edge(
            "Method:com.example.CommandService#execute/1",
            "Method:java.lang.Runtime#exec/1",
            EdgeKind::Calls,
        ),
    ];

    let rules = default_rules();
    let paths = find_taint_paths(&nodes, &edges, &rules);

    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].category, SinkCategory::Exec);
    assert_eq!(
        paths[0].sink_method.as_str(),
        "Method:com.example.CommandService#execute/1"
    );
    assert_eq!(paths[0].edge_count(), 1);
}

#[test]
fn sanitizer_stops_propagation() {
    let nodes = vec![
        method_node("Method:com.example.WebController#render/1"),
        method_node("Method:com.example.WebService#buildHtml/1"),
        method_node("Method:org.springframework.web.util.HtmlUtils#htmlEscape/1"),
    ];
    let edges = vec![
        edge(
            "Method:com.example.WebController#render/1",
            "Route:/render",
            EdgeKind::HandlesRoute,
        ),
        edge(
            "Method:com.example.WebController#render/1",
            "Method:com.example.WebService#buildHtml/1",
            EdgeKind::Calls,
        ),
        edge(
            "Method:com.example.WebService#buildHtml/1",
            "Method:org.springframework.web.util.HtmlUtils#htmlEscape/1",
            EdgeKind::Calls,
        ),
    ];

    let rules = default_rules();
    let paths = find_taint_paths(&nodes, &edges, &rules);
    assert!(
        paths.is_empty(),
        "path through sanitizer should be suppressed"
    );
}

#[test]
fn no_source_no_paths() {
    let nodes = vec![method_node("Method:com.example.Dao#save/1")];
    let edges = vec![edge(
        "Method:com.example.Dao#save/1",
        "DbQuery:Dao:5:1",
        EdgeKind::ExecutesQuery,
    )];
    let mut nodes_with_query = nodes;
    nodes_with_query.push(db_query_node("DbQuery:Dao:5:1", true));

    let rules = default_rules();
    let paths = find_taint_paths(&nodes_with_query, &edges, &rules);
    assert!(paths.is_empty(), "no source → no taint paths");
}
