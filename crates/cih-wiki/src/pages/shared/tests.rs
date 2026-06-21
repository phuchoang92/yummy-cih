use super::*;
use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

fn route_pair(method: &str, path: &str, handler_id: &str) -> (Node, Node) {
    let handler = Node {
        id: NodeId::new(handler_id.to_string()),
        kind: NodeKind::Method,
        name: handler_id.split('#').nth(1).unwrap_or("handle").to_string(),
        qualified_name: Some(handler_id.to_string()),
        file: "Controller.java".to_string(),
        range: Range::default(),
        props: None,
    };
    let route_name = format!("{} {}", method, path);
    let route = Node {
        id: NodeId::new(format!("Route:{}", route_name)),
        kind: NodeKind::Route,
        name: route_name,
        qualified_name: None,
        file: "Controller.java".to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "httpMethod": method,
            "path": path,
            "decorator": format!("{}Mapping", &method[..1]),
        })),
    };
    (handler, route)
}

fn graph_with_routes(pairs: Vec<(Node, Node)>) -> WikiGraph {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    for (handler, route) in pairs {
        edges.push(Edge {
            src: handler.id.clone(),
            dst: route.id.clone(),
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: String::new(),
                props: None,
        });
        nodes.push(handler);
        nodes.push(route);
    }
    WikiGraph::build(&nodes, &edges, &[], &[])
}

#[test]
fn render_routes_page_produces_table() {
    let (h, r) = route_pair("GET", "/api/orders", "com.example.OrderController#list/0");
    let g = graph_with_routes(vec![(h, r)]);
    let md = render_routes_page(&g);
    assert!(md.contains("---\ntitle: API Routes"), "has frontmatter");
    assert!(md.contains("| Method |"), "has table header");
    assert!(md.contains("/api/orders"), "has path");
    assert!(md.contains("`GET`"), "has method");
}

#[test]
fn render_routes_json_is_openapi() {
    let (h, r) = route_pair(
        "POST",
        "/api/orders",
        "com.example.OrderController#create/0",
    );
    let g = graph_with_routes(vec![(h, r)]);
    let val = render_routes_json(&g);
    assert_eq!(val["openapi"], "3.0.3");
    assert!(val["paths"]["/api/orders"]["post"].is_object());
}

#[test]
fn markdown_pages_include_docusaurus_frontmatter() {
    let (h, r) = route_pair("GET", "/api/health", "com.example.HealthController#check/0");
    let g = graph_with_routes(vec![(h, r)]);
    let md = render_routes_page(&g);
    assert!(
        md.starts_with("---\n"),
        "page must start with frontmatter delimiter"
    );
    assert!(md.contains("title:"), "frontmatter must contain title");
}
