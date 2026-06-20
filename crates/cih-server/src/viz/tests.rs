use super::*;
use cih_core::{NodeId, NodeKind};
use cih_graph_store::{Direction, ImpactNode};

#[test]
fn render_mermaid_flow_empty_steps() {
    let entry = NodeId::new("Method:com.example.Foo#bar/0".to_string());
    let out = render_mermaid_flow(&entry, &[]);
    assert!(out.starts_with("flowchart TD\n"));
    assert!(out.contains("nMethod_com_example_Foo_bar_0"));
    // No edges when steps is empty.
    assert!(!out.contains("-->"));
}

#[test]
fn render_mermaid_flow_two_hops() {
    let entry = NodeId::new("Method:com.example.Entry#run/0".to_string());
    let parent_id = entry.clone();
    let child_id = NodeId::new("Method:com.example.Service#save/1".to_string());
    let grandchild_id = NodeId::new("Method:com.example.Repo#insert/1".to_string());
    let steps = vec![
        FlowNode {
            id: child_id.clone(),
            kind: NodeKind::Method,
            name: "save".to_string(),
            qualified_name: None,
            file: "Service.java".to_string(),
            depth: 1,
            parent_id: Some(parent_id.clone()),
        },
        FlowNode {
            id: grandchild_id.clone(),
            kind: NodeKind::Method,
            name: "insert".to_string(),
            qualified_name: None,
            file: "Repo.java".to_string(),
            depth: 2,
            parent_id: Some(child_id.clone()),
        },
    ];
    let out = render_mermaid_flow(&entry, &steps);
    assert!(out.contains("-->"), "should have edges");
    assert!(out.contains("save"), "child label present");
    assert!(out.contains("insert"), "grandchild label present");
    // Entry → child, child → grandchild.
    let entry_key = mermaid_key(entry.as_str());
    let child_key = mermaid_key(child_id.as_str());
    let grand_key = mermaid_key(grandchild_id.as_str());
    assert!(out.contains(&format!("{} --> {}", entry_key, child_key)));
    assert!(out.contains(&format!("{} --> {}", child_key, grand_key)));
}

#[test]
fn render_d3_impact_produces_nodes_and_links() {
    let root = NodeId::new("Method:com.example.Foo#bar/0".to_string());
    let affected = vec![
        ImpactNode {
            id: NodeId::new("Method:com.example.A#x/0".to_string()),
            depth: 1,
            via: "CALLS".to_string(),
            name: "x".to_string(),
            kind: "Method".to_string(),
            parent_id: Some(root.clone()),
        },
        ImpactNode {
            id: NodeId::new("Method:com.example.B#y/0".to_string()),
            depth: 2,
            via: "CALLS".to_string(),
            name: "y".to_string(),
            kind: "Method".to_string(),
            parent_id: Some(NodeId::new("Method:com.example.A#x/0".to_string())),
        },
    ];
    let impact = Impact {
        root: root.clone(),
        direction: Direction::Upstream,
        affected,
        risk: "low".to_string(),
    };
    let val = render_d3_impact(&impact);
    let nodes = val["nodes"].as_array().unwrap();
    let links = val["links"].as_array().unwrap();
    // root + 2 affected = 3 nodes; 2 links.
    assert_eq!(nodes.len(), 3);
    assert_eq!(links.len(), 2);
    assert_eq!(val["risk"], "low");
    assert_eq!(val["format"], "d3-force");
}

#[test]
fn render_community_diagram_produces_nodes_and_links() {
    let communities = vec![
        CommunityInfo {
            id: "Community:0".to_string(),
            name: "order-service".to_string(),
            symbol_count: 42,
            cohesion: 0.73,
        },
        CommunityInfo {
            id: "Community:1".to_string(),
            name: "payment-service".to_string(),
            symbol_count: 18,
            cohesion: 0.65,
        },
    ];
    let edges = vec![CommunityEdge {
        src: "Community:0".to_string(),
        dst: "Community:1".to_string(),
        weight: 7,
    }];
    let val = render_community_diagram(&communities, &edges);
    assert_eq!(val["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(val["links"].as_array().unwrap().len(), 1);
    assert_eq!(val["links"][0]["weight"], 7);
    assert_eq!(val["format"], "d3-force");
}

#[test]
fn render_openapi_groups_by_path() {
    let routes = vec![
        RouteInfo {
            path: "/api/users/{id}".to_string(),
            http_method: "GET".to_string(),
            decorator: "GetMapping".to_string(),
            handler_id: NodeId::new("Method:com.example.UserController#getUser/1".to_string()),
            handler_name: "getUser".to_string(),
            handler_qualified: "com.example.UserController#getUser/1".to_string(),
        },
        RouteInfo {
            path: "/api/users/{id}".to_string(),
            http_method: "DELETE".to_string(),
            decorator: "DeleteMapping".to_string(),
            handler_id: NodeId::new(
                "Method:com.example.UserController#deleteUser/1".to_string(),
            ),
            handler_name: "deleteUser".to_string(),
            handler_qualified: "com.example.UserController#deleteUser/1".to_string(),
        },
    ];
    let val = render_openapi(&routes);
    assert_eq!(val["openapi"], "3.0.3");
    let paths = val["paths"].as_object().unwrap();
    // Both routes share the same path → one path key.
    assert_eq!(paths.len(), 1);
    let path_item = &paths["/api/users/{id}"];
    assert!(path_item["get"].is_object(), "GET operation present");
    assert!(path_item["delete"].is_object(), "DELETE operation present");
    assert_eq!(path_item["get"]["summary"], "getUser");
    assert_eq!(path_item["delete"]["summary"], "deleteUser");
}
