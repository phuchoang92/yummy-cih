//! Pure rendering functions that convert graph data into frontend-consumable formats.
//! No async, no store access — all inputs are already fetched by the caller.

use cih_core::NodeId;
use cih_graph_store::{CommunityEdge, CommunityInfo, FlowNode, Impact, RouteInfo};

// ---- Mermaid: trace_flow ----

/// Render a `flowchart TD` Mermaid diagram from a downstream flow trace.
/// `entry_id` is the root node (depth 0); `steps` are the downstream nodes.
/// Edges are drawn from each node's `parent_id` (or the entry if absent).
pub fn render_mermaid_flow(entry_id: &NodeId, steps: &[FlowNode]) -> String {
    let mut out = String::from("flowchart TD\n");

    // Entry node definition.
    let entry_key = mermaid_key(entry_id.as_str());
    let entry_label = short_label(entry_id.as_str(), "Entry");
    out.push_str(&format!("    {}[\"{}\"]\n", entry_key, entry_label));

    // Node definitions.
    for step in steps {
        let key = mermaid_key(step.id.as_str());
        let label = truncate(&format!("{}\n{}", step.kind.label(), step.name), 60);
        out.push_str(&format!("    {}[\"{}\"]\n", key, escape_mermaid(&label)));
    }

    // Edges.
    for step in steps {
        let child_key = mermaid_key(step.id.as_str());
        let parent_key = step
            .parent_id
            .as_ref()
            .map(|p| mermaid_key(p.as_str()))
            .unwrap_or_else(|| entry_key.clone());
        out.push_str(&format!("    {} --> {}\n", parent_key, child_key));
    }

    out
}

/// Stable, Mermaid-safe identifier derived from a NodeId string.
fn mermaid_key(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("n{}", sanitized)
}

/// Short human label from a NodeId (takes the part after the last `#` or `:`,
/// falling back to the full string).
fn short_label(id: &str, fallback: &str) -> String {
    id.rsplit(['#', ':']).next().unwrap_or(fallback).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{}…", cut)
    }
}

fn escape_mermaid(s: &str) -> String {
    s.replace('"', "#quot;")
}

// ---- D3 force-directed JSON: impact ----

/// Render a D3 force-directed graph JSON from an `Impact` result.
/// Format: `{ format, risk, nodes: [{id, label, kind, depth}], links: [{source, target, label}] }`
pub fn render_d3_impact(impact: &Impact) -> serde_json::Value {
    let mut nodes: Vec<serde_json::Value> = vec![serde_json::json!({
        "id": impact.root.as_str(),
        "label": short_label(impact.root.as_str(), "root"),
        "kind": "root",
        "depth": 0,
    })];
    let mut links: Vec<serde_json::Value> = Vec::new();

    for node in &impact.affected {
        nodes.push(serde_json::json!({
            "id": node.id.as_str(),
            "label": if node.name.is_empty() { short_label(node.id.as_str(), "") } else { node.name.clone() },
            "kind": node.kind,
            "depth": node.depth,
        }));
        let source = node
            .parent_id
            .as_ref()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| impact.root.as_str().to_string());
        links.push(serde_json::json!({
            "source": source,
            "target": node.id.as_str(),
            "label": node.via,
        }));
    }

    serde_json::json!({
        "format": "d3-force",
        "risk": impact.risk,
        "direction": format!("{:?}", impact.direction).to_lowercase(),
        "nodes": nodes,
        "links": links,
    })
}

// ---- D3 force-directed JSON: communities ----

/// Render a community service-map diagram.
/// Format: `{ format, nodes: [{id, label, symbol_count, cohesion}], links: [{source, target, weight}] }`
pub fn render_community_diagram(
    communities: &[CommunityInfo],
    edges: &[CommunityEdge],
) -> serde_json::Value {
    let nodes: Vec<serde_json::Value> = communities
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "label": c.name,
                "symbol_count": c.symbol_count,
                "cohesion": (c.cohesion * 1000.0).round() / 1000.0,
            })
        })
        .collect();

    let links: Vec<serde_json::Value> = edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "source": e.src,
                "target": e.dst,
                "weight": e.weight,
            })
        })
        .collect();

    serde_json::json!({
        "format": "d3-force",
        "nodes": nodes,
        "links": links,
    })
}

// ---- OpenAPI 3.0: route_map ----

/// Render an OpenAPI 3.0.3 JSON object from a list of `RouteInfo` records.
/// Schemas are omitted (not available at this layer); `x-handler-*` extensions
/// give the yummy frontend enough to deep-link into the code graph.
pub fn render_openapi(routes: &[RouteInfo]) -> serde_json::Value {
    use std::collections::BTreeMap;

    // Group by path → method (BTreeMap keeps paths sorted).
    let mut paths: BTreeMap<String, BTreeMap<String, serde_json::Value>> = BTreeMap::new();

    for route in routes {
        let method = route.http_method.to_lowercase();
        let op_id = make_operation_id(&method, &route.path);
        let handler_class = route
            .handler_qualified
            .split('#')
            .next()
            .unwrap_or(&route.handler_qualified)
            .to_string();

        let operation = serde_json::json!({
            "operationId": op_id,
            "summary": route.handler_name,
            "x-handler-id": route.handler_id.as_str(),
            "x-handler-class": handler_class,
            "x-decorator": route.decorator,
            "responses": {
                "200": { "description": "OK" }
            }
        });

        paths
            .entry(route.path.clone())
            .or_default()
            .insert(method, operation);
    }

    let paths_value: serde_json::Value = paths
        .into_iter()
        .map(|(path, methods)| {
            (
                path,
                serde_json::Value::Object(methods.into_iter().collect::<serde_json::Map<_, _>>()),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();

    serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Indexed API Surface",
            "version": "1.0.0",
            "description": "Generated from the CIH code-intelligence graph. \
                            Request/response schemas are not available at this layer."
        },
        "paths": paths_value,
    })
}

/// Derive a unique, readable operationId from HTTP method + path.
/// e.g. GET /api/users/{id} → "get_api_users_id"
fn make_operation_id(method: &str, path: &str) -> String {
    let parts: String = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            // Strip braces from path variables: {id} → id
            seg.trim_matches(|c| c == '{' || c == '}')
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("_");
    let id = format!("{}_{}", method, parts);
    // Clamp to 64 chars to keep operationIds readable.
    truncate(&id, 64)
}

// ---- Tests ----

#[cfg(test)]
mod tests {
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
}
