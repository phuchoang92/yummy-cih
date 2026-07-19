use cih_core::{Node, NodeId, NodeKind, Range};
use cih_graph_store::{Direction, FlowHop, FlowNode};
use cih_server::browser::{
    bounded_depth, limit_or_default, overview_limit, parse_graph_direction, render_flow_graph,
    INDEX_HTML, OVERVIEW_DEFAULT_EDGES, OVERVIEW_DEFAULT_NODES, OVERVIEW_MAX_EDGES,
    OVERVIEW_MAX_NODES,
};

#[test]
fn browser_limits_are_bounded() {
    assert_eq!(limit_or_default(None, 200, 1000), 200);
    assert_eq!(limit_or_default(Some(0), 200, 1000), 1);
    assert_eq!(limit_or_default(Some(50), 200, 1000), 50);
    assert_eq!(limit_or_default(Some(10_000), 200, 1000), 1000);

    assert_eq!(bounded_depth(None, 6, 10), 6);
    assert_eq!(bounded_depth(Some(0), 6, 10), 1);
    assert_eq!(bounded_depth(Some(12), 6, 10), 10);
    assert_eq!(
        overview_limit(None, OVERVIEW_DEFAULT_NODES, OVERVIEW_MAX_NODES),
        OVERVIEW_DEFAULT_NODES
    );
    assert_eq!(
        overview_limit(Some(80_000), OVERVIEW_DEFAULT_NODES, OVERVIEW_MAX_NODES),
        OVERVIEW_MAX_NODES
    );
    assert_eq!(
        overview_limit(Some(0), OVERVIEW_DEFAULT_EDGES, OVERVIEW_MAX_EDGES),
        1
    );
}

#[test]
fn graph_direction_defaults_to_upstream() {
    assert_eq!(parse_graph_direction(None), Direction::Upstream);
    assert_eq!(
        parse_graph_direction(Some("downstream")),
        Direction::Downstream
    );
    assert_eq!(parse_graph_direction(Some("both")), Direction::Both);
    assert_eq!(parse_graph_direction(Some("unknown")), Direction::Upstream);
}

#[test]
fn flow_graph_response_contains_d3_and_mermaid_shapes() {
    let entry = NodeId::new("Method:com.acme.Controller#run/0");
    let child = NodeId::new("Method:com.acme.Service#save/1");
    let steps = vec![FlowHop {
        node: FlowNode {
            id: child.clone(),
            kind: NodeKind::Method,
            name: "save".into(),
            qualified_name: Some("com.acme.Service#save/1".into()),
            file: "src/main/java/com/acme/Service.java".into(),
            depth: 1,
            parent_id: Some(entry.clone()),
            intercepted_by: Vec::new(),
        },
        via: None,
    }];
    let entry_node = Node {
        id: entry.clone(),
        kind: NodeKind::Method,
        name: "run".into(),
        qualified_name: Some("com.acme.Controller#run/0".into()),
        file: "src/main/java/com/acme/Controller.java".into(),
        range: Range::default(),
        props: None,
    };

    let value = render_flow_graph(&entry, Some(&entry_node), &steps, 6);

    assert_eq!(value["format"], "d3-force");
    assert_eq!(value["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(value["links"].as_array().unwrap().len(), 1);
    assert!(value["mermaid"]
        .as_str()
        .unwrap()
        .starts_with("flowchart TD"));
}

#[test]
fn graph_shell_has_browser_mount_points() {
    assert!(INDEX_HTML.contains("cih-graph-browser"));
    assert!(INDEX_HTML.contains("/graph/assets/app.js"));
}
