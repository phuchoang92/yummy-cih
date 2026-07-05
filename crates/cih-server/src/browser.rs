//! Local graph browser HTTP routes.
//!
//! This is intentionally CIH-only and read-only. It serves a small static UI and
//! bounded JSON endpoints backed by the existing `GraphStore` domain methods.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use cih_core::{Node, NodeId};
use cih_graph_store::{Direction, FlowHop, GraphStore, GraphStoreError, GraphSummary};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::layout;
use crate::search::{self, QueryResult, SearchState};
use crate::viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};

#[doc(hidden)]
pub const INDEX_HTML: &str = include_str!("../assets/graph/index.html");
const APP_JS: &str = include_str!("../assets/graph/app.js");
const STYLES_CSS: &str = include_str!("../assets/graph/styles.css");

#[derive(Clone)]
pub(crate) struct BrowserState {
    store: Arc<dyn GraphStore>,
    search: SearchState,
    /// Artifacts root (`CIH_ARTIFACTS_DIR`, e.g. `<repo>/.cih/artifacts`). Used to locate the
    /// sibling embedding-cluster artifacts under `<repo>/.cih/artifacts-features/<version>`.
    artifacts_dir: Option<PathBuf>,
}

impl BrowserState {
    pub(crate) fn new(
        store: Arc<dyn GraphStore>,
        search: SearchState,
        artifacts_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            store,
            search,
            artifacts_dir,
        }
    }
}

pub(crate) fn router(state: BrowserState) -> Router {
    Router::new()
        .route("/graph", get(graph_shell))
        .route("/graph/", get(graph_shell))
        .route("/graph/assets/app.js", get(app_js))
        .route("/graph/assets/styles.css", get(styles_css))
        .route("/api/graph/summary", get(graph_summary_handler))
        .route("/api/graph/overview", get(graph_overview))
        .route("/api/graph/search", get(graph_search))
        .route("/api/graph/context", get(graph_context))
        .route("/api/graph/impact", get(graph_impact))
        .route("/api/graph/flow", get(graph_flow))
        .route("/api/graph/communities", get(graph_communities))
        .route("/api/graph/features", get(graph_features))
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
struct OverviewParams {
    max_nodes: Option<usize>,
    max_edges: Option<usize>,
    /// Comma-separated list of node kinds to include (e.g. "Community,Process,Route").
    /// When absent the server auto-selects a structural + high-degree projection.
    kinds: Option<String>,
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

#[doc(hidden)]
pub const OVERVIEW_DEFAULT_NODES: usize = 5_000;
#[doc(hidden)]
pub const OVERVIEW_MAX_NODES: usize = 20_000;
#[doc(hidden)]
pub const OVERVIEW_DEFAULT_EDGES: usize = 25_000;
#[doc(hidden)]
pub const OVERVIEW_MAX_EDGES: usize = 100_000;

async fn graph_overview(
    State(state): State<BrowserState>,
    Query(params): Query<OverviewParams>,
) -> Result<Json<layout::LayoutOverview>, BrowserError> {
    let max_nodes = overview_limit(
        params.max_nodes,
        OVERVIEW_DEFAULT_NODES,
        OVERVIEW_MAX_NODES,
    );
    let max_edges = overview_limit(
        params.max_edges,
        OVERVIEW_DEFAULT_EDGES,
        OVERVIEW_MAX_EDGES,
    );
    let kinds: Option<Vec<String>> = params.kinds.as_deref().map(|raw| {
        raw.split(',').map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()).collect()
    });
    let overview = state
        .store
        .graph_overview(max_nodes, max_edges, kinds.as_deref())
        .await
        .map_err(BrowserError::from_store)?;
    let positioned = tokio::task::spawn_blocking(move || layout::compute(overview))
        .await
        .map_err(|err| BrowserError::internal(format!("layout worker failed: {err}")))?;
    Ok(Json(positioned))
}

async fn graph_summary_handler(
    State(state): State<BrowserState>,
) -> Result<Json<GraphSummary>, BrowserError> {
    let summary = state
        .store
        .graph_summary()
        .await
        .map_err(BrowserError::from_store)?;
    Ok(Json(summary))
}

async fn graph_search(
    State(state): State<BrowserState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<QueryResult>, BrowserError> {
    let q = params.q.trim();
    if q.is_empty() {
        return Err(BrowserError::bad_request("query parameter `q` is required"));
    }

    let limit = search::query_limit(params.limit.unwrap_or(0));
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

/// Embedding clusters (feature groups) for the current repo. Reads the
/// `groups.jsonl` artifact written by `cih-engine discover --feature-strategy embed`
/// and returns clusters with their member nodes, sorted so low-confidence outliers
/// surface first — the signal for eyeballing grouping quality.
///
/// Always returns 200: when no embedding run exists it returns an empty list plus a
/// `note` explaining how to generate one, so the UI can render a friendly empty state.
async fn graph_features(State(state): State<BrowserState>) -> Json<serde_json::Value> {
    match load_feature_clusters(state.artifacts_dir.as_deref()) {
        Ok(clusters) => Json(json!({ "clusters": clusters })),
        Err(err) => Json(json!({ "clusters": [], "note": err.to_string() })),
    }
}

#[derive(Serialize)]
struct ClusterMember {
    node_id: String,
    confidence: f32,
    evidence: String,
    strategy: String,
    pinned: bool,
}

#[derive(Serialize)]
struct ClusterInfo {
    name: String,
    node_count: usize,
    avg_confidence: f32,
    members: Vec<ClusterMember>,
}

fn load_feature_clusters(artifacts_dir: Option<&Path>) -> anyhow::Result<Vec<ClusterInfo>> {
    let Some(dir) = artifacts_dir else {
        anyhow::bail!(
            "CIH_ARTIFACTS_DIR is not set — cannot locate embedding-cluster artifacts"
        );
    };
    // `dir` is the artifacts root (`<repo>/.cih/artifacts`); the source graph version names the
    // sibling `<repo>/.cih/artifacts-features/<version>` that discover writes clusters into.
    let artifacts = cih_core::GraphArtifacts::latest_in_dir(dir)?;
    let version = artifacts.version.0.clone();
    let repo = dir.parent().and_then(Path::parent).ok_or_else(|| {
        anyhow::anyhow!("cannot derive repo root from artifacts dir {}", dir.display())
    })?;
    let feat_dir = cih_grouping::find_feature_artifact_dir(repo, &version).ok_or_else(|| {
        anyhow::anyhow!(
            "no embedding clusters found for graph version {version} — run \
             `cih-engine discover <repo> --feature-strategy embed` first"
        )
    })?;
    let entries = cih_grouping::read_feature_artifact(&feat_dir)?;
    Ok(build_clusters(entries))
}

fn build_clusters(entries: Vec<cih_grouping::FeatureGroupEntry>) -> Vec<ClusterInfo> {
    use std::collections::BTreeMap;

    let mut by_name: BTreeMap<String, Vec<cih_grouping::FeatureGroupEntry>> = BTreeMap::new();
    for entry in entries {
        by_name.entry(entry.name.clone()).or_default().push(entry);
    }

    let mut clusters: Vec<ClusterInfo> = by_name
        .into_iter()
        .map(|(name, mut group)| {
            // Ascending confidence → weakly-attached (outlier) members appear first.
            group.sort_by(|a, b| {
                a.confidence
                    .partial_cmp(&b.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let node_count = group.len();
            let avg_confidence = if node_count == 0 {
                0.0
            } else {
                group.iter().map(|e| e.confidence).sum::<f32>() / node_count as f32
            };
            let members = group
                .into_iter()
                .map(|e| ClusterMember {
                    node_id: e.node_id,
                    confidence: e.confidence,
                    evidence: e.evidence,
                    strategy: e.strategy,
                    pinned: e.pinned,
                })
                .collect();
            ClusterInfo {
                name,
                node_count,
                avg_confidence,
                members,
            }
        })
        .collect();

    // Largest clusters first; stable tiebreak on name.
    clusters.sort_by(|a, b| b.node_count.cmp(&a.node_count).then(a.name.cmp(&b.name)));
    clusters
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

#[doc(hidden)]
pub fn render_flow_graph(
    entry_id: &NodeId,
    entry_node: Option<&Node>,
    hops: &[FlowHop],
    depth_limit: u32,
) -> serde_json::Value {
    let mut nodes = Vec::with_capacity(hops.len() + 1);
    let mut links = Vec::with_capacity(hops.len());

    nodes.push(json!({
        "id": entry_id.as_str(),
        "label": entry_node
            .map(|node| node_label(&node.name, node.qualified_name.as_deref(), entry_id.as_str()))
            .unwrap_or_else(|| short_label(entry_id.as_str())),
        "kind": entry_node.map(|node| node.kind.label()).unwrap_or("Entry"),
        "depth": 0,
        "file": entry_node.map(|node| node.file.as_str()).unwrap_or(""),
    }));

    for hop in hops.iter() {
        let step = &hop.node;
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
            "label": hop.via.as_ref().map(|v| v.kind.as_str()).unwrap_or("flow"),
        }));
    }

    json!({
        "format": "d3-force",
        "entry_point": entry_id.as_str(),
        "depth_limit": depth_limit,
        "nodes": nodes,
        "links": links,
        "mermaid": render_mermaid_flow(entry_id, hops),
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

#[doc(hidden)]
pub fn parse_graph_direction(raw: Option<&str>) -> Direction {
    match raw.unwrap_or("upstream").to_ascii_lowercase().as_str() {
        "downstream" => Direction::Downstream,
        "both" => Direction::Both,
        _ => Direction::Upstream,
    }
}

#[doc(hidden)]
pub fn bounded_depth(raw: Option<u32>, default: u32, max: u32) -> u32 {
    raw.unwrap_or(default).clamp(1, max)
}

#[doc(hidden)]
pub fn limit_or_default(raw: Option<usize>, default: usize, max: usize) -> usize {
    raw.unwrap_or(default).clamp(1, max)
}

#[doc(hidden)]
pub fn overview_limit(raw: Option<usize>, default: usize, max: usize) -> usize {
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
    use cih_grouping::FeatureGroupEntry;

    fn entry(name: &str, node_id: &str, confidence: f32) -> FeatureGroupEntry {
        FeatureGroupEntry {
            id: format!("feature:{name}"),
            name: name.to_string(),
            node_id: node_id.to_string(),
            strategy: "embed".to_string(),
            confidence,
            pinned: false,
            evidence: String::new(),
            node_content_hash: 0,
        }
    }

    #[test]
    fn build_clusters_orders_by_size_then_confidence() {
        let clusters = build_clusters(vec![
            entry("payment", "Class:a.Pay", 0.9),
            entry("payment", "Class:a.PayHelper", 0.3),
            entry("order", "Class:a.Order", 0.8),
        ]);

        // Largest cluster first.
        assert_eq!(clusters[0].name, "payment");
        assert_eq!(clusters[0].node_count, 2);
        assert_eq!(clusters[1].name, "order");

        // Members sorted ascending by confidence — outlier surfaces first.
        assert_eq!(clusters[0].members[0].node_id, "Class:a.PayHelper");
        assert!(clusters[0].members[0].confidence < clusters[0].members[1].confidence);

        // Average confidence is the mean of the members.
        assert!((clusters[0].avg_confidence - 0.6).abs() < 1e-6);
    }
}

