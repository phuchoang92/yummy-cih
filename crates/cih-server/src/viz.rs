//! Pure rendering functions that convert graph data into frontend-consumable formats.
//! No async, no store access — all inputs are already fetched by the caller.

use cih_core::NodeId;
use cih_graph_store::{CommunityEdge, CommunityInfo, FlowHop, Impact, RouteInfo};

// ---- Mermaid: trace_flow ----

/// Render a `flowchart TD` Mermaid diagram from a downstream flow trace.
/// `hops[0]` is the root entry point (via = None); subsequent hops are downstream nodes.
/// Edges are drawn from each node's `parent_id` (or the entry if absent).
pub fn render_mermaid_flow(entry_id: &NodeId, hops: &[FlowHop]) -> String {
    let mut out = String::from("flowchart TD\n");

    // Entry node definition.
    let entry_key = mermaid_key(entry_id.as_str());
    let entry_label = short_label(entry_id.as_str(), "Entry");
    out.push_str(&format!("    {}[\"{}\"]\n", entry_key, entry_label));

    // Node definitions.
    for hop in hops.iter() {
        let step = &hop.node;
        let key = mermaid_key(step.id.as_str());
        let label = truncate(&format!("{}\n{}", step.kind.label(), step.name), 60);
        out.push_str(&format!("    {}[\"{}\"]\n", key, escape_mermaid(&label)));
    }

    // Edges.
    for hop in hops.iter() {
        let step = &hop.node;
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
#[doc(hidden)]
pub fn mermaid_key(id: &str) -> String {
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
