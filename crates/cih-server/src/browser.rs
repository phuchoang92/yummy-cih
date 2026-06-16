//! Local graph browser HTTP routes.
//!
//! This is intentionally CIH-only and read-only. It serves a small static UI and
//! bounded JSON endpoints backed by the existing `GraphStore` domain methods.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use cih_core::{Node, NodeId};
use cih_graph_store::{Direction, FlowNode, GraphStore, GraphStoreError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::search::{self, QueryResult, SearchState};
use crate::viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};

const INDEX_HTML: &str = include_str!("../assets/graph/index.html");
const APP_JS: &str = include_str!("../assets/graph/app.js");
const STYLES_CSS: &str = include_str!("../assets/graph/styles.css");

#[derive(Clone)]
pub(crate) struct BrowserState {
    store: Arc<dyn GraphStore>,
    search: SearchState,
}

impl BrowserState {
    pub(crate) fn new(store: Arc<dyn GraphStore>, search: SearchState) -> Self {
        Self { store, search }
    }
}

pub(crate) fn router(state: BrowserState) -> Router {
    Router::new()
        .route("/graph", get(graph_shell))
        .route("/graph/", get(graph_shell))
        .route("/graph/assets/app.js", get(app_js))
        .route("/graph/assets/styles.css", get(styles_css))
        .route("/api/graph/search", get(graph_search))
        .route("/api/graph/context", get(graph_context))
        .route("/api/graph/impact", get(graph_impact))
        .route("/api/graph/flow", get(graph_flow))
        .route("/api/graph/communities", get(graph_communities))
        .route("/api/graph/routes", get(graph_routes))
        .with_state(state)
}

async fn graph_shell() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    text_asset("application/javascript; charset=utf-8", APP_JS)
}

async fn styles_css() -> Response {
    text_asset("text/css; charset=utf-8", STYLES_CSS)
}

fn text_asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<usize>,
    /// Include one-hop graph around top hits. Defaults to true for the browser.
    expand: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct NodeParams {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ImpactParams {
    id: String,
    direction: Option<String>,
    depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FlowParams {
    id: String,
    depth: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RoutesParams {
    prefix: Option<String>,
    limit: Option<usize>,
}

async fn graph_search(
    State(state): State<BrowserState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<QueryResult>, BrowserError> {
    let q = params.q.trim();
    if q.is_empty() {
        return Err(BrowserError::bad_request("query parameter `q` is required"));
    }

    let limit = search::query_limit(params.limit);
    let hits = state
        .search
        .query_hits(q, limit)
        .await
        .map_err(|err| BrowserError::internal(err.to_string()))?;
    let subgraph = if params.expand.unwrap_or(true) && !hits.is_empty() {
        let seeds: Vec<NodeId> = hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
        Some(
            state
                .store
                .subgraph(&seeds, 1)
                .await
                .map_err(BrowserError::from_store)?,
        )
    } else {
        None
    };

    Ok(Json(QueryResult { hits, subgraph }))
}

async fn graph_context(
    State(state): State<BrowserState>,
    Query(params): Query<NodeParams>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let id = node_id(params.id)?;
    let ctx = state
        .store
        .context(&id)
        .await
        .map_err(BrowserError::from_store)?;
    Ok(Json(json!(ctx)))
}

async fn graph_impact(
    State(state): State<BrowserState>,
    Query(params): Query<ImpactParams>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let id = node_id(params.id)?;
    let direction = parse_graph_direction(params.direction.as_deref());
    let depth = bounded_depth(params.depth, 4, 8);
    let impact = state
        .store
        .impact(&id, direction, depth)
        .await
        .map_err(BrowserError::from_store)?;

    Ok(Json(render_d3_impact(&impact)))
}

async fn graph_flow(
    State(state): State<BrowserState>,
    Query(params): Query<FlowParams>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let entry_id = node_id(params.id)?;
    let depth = bounded_depth(params.depth, 6, 10);
    let steps = state
        .store
        .flow_downstream(&entry_id, depth)
        .await
        .map_err(BrowserError::from_store)?;
    let entry_node = state
        .store
        .get_node(&entry_id)
        .await
        .map_err(BrowserError::from_store)?;

    Ok(Json(render_flow_graph(
        &entry_id,
        entry_node.as_ref(),
        &steps,
        depth,
    )))
}

async fn graph_communities(
    State(state): State<BrowserState>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let communities = state
        .store
        .communities()
        .await
        .map_err(BrowserError::from_store)?;
    let edges = state
        .store
        .community_graph()
        .await
        .map_err(BrowserError::from_store)?;
    Ok(Json(render_community_diagram(&communities, &edges)))
}

async fn graph_routes(
    State(state): State<BrowserState>,
    Query(params): Query<RoutesParams>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let prefix = params.prefix.as_deref().filter(|s| !s.trim().is_empty());
    let limit = limit_or_default(params.limit, 200, 1000);
    let routes = state
        .store
        .route_map(prefix, limit)
        .await
        .map_err(BrowserError::from_store)?;
    let openapi = render_openapi(&routes);
    Ok(Json(json!({
        "routes": routes,
        "openapi": openapi,
    })))
}

fn render_flow_graph(
    entry_id: &NodeId,
    entry_node: Option<&Node>,
    steps: &[FlowNode],
    depth_limit: u32,
) -> serde_json::Value {
    let mut nodes = Vec::with_capacity(steps.len() + 1);
    let mut links = Vec::with_capacity(steps.len());

    nodes.push(json!({
        "id": entry_id.as_str(),
        "label": entry_node
            .map(|node| node_label(&node.name, node.qualified_name.as_deref(), entry_id.as_str()))
            .unwrap_or_else(|| short_label(entry_id.as_str())),
        "kind": entry_node.map(|node| node.kind.label()).unwrap_or("Entry"),
        "depth": 0,
        "file": entry_node.map(|node| node.file.as_str()).unwrap_or(""),
    }));

    for step in steps {
        nodes.push(json!({
            "id": step.id.as_str(),
            "label": node_label(&step.name, step.qualified_name.as_deref(), step.id.as_str()),
            "kind": step.kind.label(),
            "depth": step.depth,
            "file": step.file,
        }));

        links.push(json!({
            "source": step
                .parent_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_else(|| entry_id.as_str()),
            "target": step.id.as_str(),
            "label": "flow",
        }));
    }

    json!({
        "format": "d3-force",
        "entry_point": entry_id.as_str(),
        "depth_limit": depth_limit,
        "nodes": nodes,
        "links": links,
        "mermaid": render_mermaid_flow(entry_id, steps),
    })
}

fn node_label(name: &str, qualified: Option<&str>, fallback: &str) -> String {
    if !name.trim().is_empty() {
        name.to_string()
    } else if let Some(qualified) = qualified.filter(|q| !q.trim().is_empty()) {
        short_label(qualified)
    } else {
        short_label(fallback)
    }
}

fn short_label(id: &str) -> String {
    id.rsplit(['#', ':']).next().unwrap_or(id).to_string()
}

fn node_id(raw: String) -> Result<NodeId, BrowserError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Err(BrowserError::bad_request(
            "query parameter `id` is required",
        ))
    } else {
        Ok(NodeId::new(trimmed.to_string()))
    }
}

fn parse_graph_direction(raw: Option<&str>) -> Direction {
    match raw.unwrap_or("upstream").to_ascii_lowercase().as_str() {
        "downstream" => Direction::Downstream,
        "both" => Direction::Both,
        _ => Direction::Upstream,
    }
}

fn bounded_depth(raw: Option<u32>, default: u32, max: u32) -> u32 {
    raw.unwrap_or(default).clamp(1, max)
}

fn limit_or_default(raw: Option<usize>, default: usize, max: usize) -> usize {
    raw.unwrap_or(default).clamp(1, max)
}

#[derive(Debug)]
struct BrowserError {
    status: StatusCode,
    message: String,
}

impl BrowserError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn from_store(err: GraphStoreError) -> Self {
        match err {
            GraphStoreError::NotFound(id) => Self {
                status: StatusCode::NOT_FOUND,
                message: format!("node not found: {id}"),
            },
            other => Self::internal(other.to_string()),
        }
    }
}

impl IntoResponse for BrowserError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorPayload {
            error: String,
        }

        (
            self.status,
            Json(ErrorPayload {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{NodeKind, Range};

    #[test]
    fn browser_limits_are_bounded() {
        assert_eq!(limit_or_default(None, 200, 1000), 200);
        assert_eq!(limit_or_default(Some(0), 200, 1000), 1);
        assert_eq!(limit_or_default(Some(50), 200, 1000), 50);
        assert_eq!(limit_or_default(Some(10_000), 200, 1000), 1000);

        assert_eq!(bounded_depth(None, 6, 10), 6);
        assert_eq!(bounded_depth(Some(0), 6, 10), 1);
        assert_eq!(bounded_depth(Some(12), 6, 10), 10);
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
        let steps = vec![FlowNode {
            id: child.clone(),
            kind: NodeKind::Method,
            name: "save".into(),
            qualified_name: Some("com.acme.Service#save/1".into()),
            file: "src/main/java/com/acme/Service.java".into(),
            depth: 1,
            parent_id: Some(entry.clone()),
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
}
