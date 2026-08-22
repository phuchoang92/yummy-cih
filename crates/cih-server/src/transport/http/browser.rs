//! Local graph browser HTTP routes.
//!
//! This is intentionally CIH-only and read-only. It serves the embedded graph
//! explorer UI (a React 19 + D3/Canvas single-page app built from `graph-ui/` and
//! baked in via `include_str!`) plus bounded JSON endpoints backed by the existing
//! `GraphStore` domain methods.

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use cih_core::{EdgeKind, Node, NodeId};
use cih_graph_store::{
    Direction, FlowHop, GraphProjection, GraphProjectionEdge, GraphProjectionNode,
    GraphProjectionQuery, GraphSummary, ProjectionNodeRole, ProjectionScope, SubgraphFilter,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::application::browser::{BrowserSearchResult, GraphBrowserService};
use crate::application::search::ExpansionLimits;
use crate::domain::error::AppError;
use crate::layout;
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking, run_blocking_heavy};
use crate::search;
use crate::viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};

#[doc(hidden)]
pub const INDEX_HTML: &str = include_str!("../../../assets/graph/index.html");
const APP_JS: &str = include_str!("../../../assets/graph/app.js");
const STYLES_CSS: &str = include_str!("../../../assets/graph/styles.css");

#[derive(Clone)]
pub(crate) struct BrowserState {
    queries: GraphBrowserService,
    /// Artifacts root (`CIH_ARTIFACTS_DIR`, e.g. `<repo>/.cih/artifacts`). Used to locate the
    /// sibling embedding-cluster artifacts under `<repo>/.cih/artifacts-features/<version>`.
    artifacts_dir: Option<PathBuf>,
}

impl BrowserState {
    pub(crate) fn new(queries: GraphBrowserService, artifacts_dir: Option<PathBuf>) -> Self {
        Self {
            queries,
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
        .route("/api/graph/projection", get(graph_projection))
        .route("/api/graph/projection/expand", get(graph_projection_expand))
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
    /// Aggregate expanded-result node cap (default 500, hard maximum 5000).
    max_nodes: Option<usize>,
    /// Aggregate expanded-result edge cap (default 1000, hard maximum 10000).
    max_edges: Option<usize>,
    /// Logical JSON response cap (default 256 KiB, hard maximum 1 MiB).
    max_response_bytes: Option<usize>,
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
struct ProjectionParams {
    scope: Option<String>,
    parent_id: Option<String>,
    /// Comma-separated stored relationship labels.
    edge_kinds: Option<String>,
    max_nodes: Option<usize>,
    max_edges: Option<usize>,
    max_response_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ProjectionExpandParams {
    id: String,
    edge_kinds: Option<String>,
    max_nodes: Option<usize>,
    max_edges: Option<usize>,
    max_response_bytes: Option<usize>,
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
pub const OVERVIEW_MAX_NODES: usize = crate::application::browser::OVERVIEW_NODE_CAP;
#[doc(hidden)]
pub const OVERVIEW_DEFAULT_EDGES: usize = 25_000;
#[doc(hidden)]
pub const OVERVIEW_MAX_EDGES: usize = crate::application::browser::OVERVIEW_EDGE_CAP;

const PROJECTION_REPOSITORY_DEFAULT_NODES: usize = 2_000;
const PROJECTION_REPOSITORY_DEFAULT_EDGES: usize = 5_000;
const PROJECTION_DETAIL_DEFAULT_NODES: usize = 5_000;
const PROJECTION_DETAIL_DEFAULT_EDGES: usize = 20_000;
const PROJECTION_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const PROJECTION_EXPAND_DEFAULT_NODES: usize = 250;
const PROJECTION_EXPAND_DEFAULT_EDGES: usize = 1_000;
const PROJECTION_EXPAND_MAX_NODES: usize = 1_000;
const PROJECTION_EXPAND_MAX_EDGES: usize = 5_000;
const PROJECTION_EXPAND_DEFAULT_BYTES: usize = 256 * 1024;

async fn graph_overview(
    State(state): State<BrowserState>,
    Query(params): Query<OverviewParams>,
) -> Result<Json<layout::LayoutOverview>, BrowserError> {
    let max_nodes = overview_limit(params.max_nodes, OVERVIEW_DEFAULT_NODES, OVERVIEW_MAX_NODES);
    let max_edges = overview_limit(params.max_edges, OVERVIEW_DEFAULT_EDGES, OVERVIEW_MAX_EDGES);
    let kinds: Option<Vec<String>> = params.kinds.as_deref().map(|raw| {
        raw.split(',')
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
            .collect()
    });
    let overview = state
        .queries
        .overview(max_nodes, max_edges, kinds.as_deref())
        .await
        .map_err(BrowserError::from_app)?;
    let positioned = run_blocking(blocking_timeout(), "graph layout", move || {
        layout::compute(overview)
    })
    .await
    .map_err(|err| BrowserError::internal(err.to_string()))?;
    Ok(Json(positioned))
}

async fn graph_projection(
    State(state): State<BrowserState>,
    Query(params): Query<ProjectionParams>,
) -> Result<Json<layout::LayoutProjection>, BrowserError> {
    let scope = parse_projection_scope(params.scope.as_deref())?;
    let parent_id = match params.parent_id {
        Some(id) => Some(node_id(id)?),
        None => None,
    };
    let (default_nodes, default_edges) = if scope == ProjectionScope::Repository {
        (
            PROJECTION_REPOSITORY_DEFAULT_NODES,
            PROJECTION_REPOSITORY_DEFAULT_EDGES,
        )
    } else {
        (
            PROJECTION_DETAIL_DEFAULT_NODES,
            PROJECTION_DETAIL_DEFAULT_EDGES,
        )
    };
    let query = GraphProjectionQuery {
        scope,
        parent_id: parent_id.clone(),
        edge_kinds: parse_projection_edge_kinds(params.edge_kinds.as_deref())?,
        node_limit: limit_or_default(
            params.max_nodes,
            default_nodes,
            cih_graph_store::PROJECTION_NODE_LIMIT,
        ),
        edge_limit: limit_or_default(
            params.max_edges,
            default_edges,
            cih_graph_store::PROJECTION_EDGE_LIMIT,
        ),
        boundary_limit: cih_graph_store::PROJECTION_BOUNDARY_LIMIT,
    };
    query
        .validate()
        .map_err(|error| BrowserError::bad_request(error.to_string()))?;
    let result = state
        .queries
        .projection(query)
        .await
        .map_err(BrowserError::from_app)?;
    let response = layout::compute_projection(
        result.graph.as_ref().clone(),
        scope,
        parent_id.map(|id| id.to_string()),
        result.publication_epoch,
    );
    let max_response_bytes =
        projection_response_limit(params.max_response_bytes, PROJECTION_MAX_RESPONSE_BYTES);
    Ok(Json(contain_projection_response(
        response,
        max_response_bytes,
    )))
}

async fn graph_projection_expand(
    State(state): State<BrowserState>,
    Query(params): Query<ProjectionExpandParams>,
) -> Result<Json<layout::LayoutProjection>, BrowserError> {
    let id = node_id(params.id)?;
    let mut edge_kinds = parse_projection_edge_kinds(params.edge_kinds.as_deref())?;
    if edge_kinds.is_empty() {
        edge_kinds = cih_graph_store::default_projection_edge_kinds();
    }
    let filter = SubgraphFilter {
        radius: 1,
        node_limit: limit_or_default(
            params.max_nodes,
            PROJECTION_EXPAND_DEFAULT_NODES,
            PROJECTION_EXPAND_MAX_NODES,
        ),
        edge_limit: limit_or_default(
            params.max_edges,
            PROJECTION_EXPAND_DEFAULT_EDGES,
            PROJECTION_EXPAND_MAX_EDGES,
        ),
        transition_page_limit: cih_graph_store::TRANSITION_PAGE_MAX,
        edge_kinds,
    };
    let result = state
        .queries
        .expand(id.clone(), filter)
        .await
        .map_err(BrowserError::from_app)?;
    let graph = expansion_projection(result.graph);
    let response = layout::compute_projection(
        graph,
        ProjectionScope::File,
        Some(id.to_string()),
        result.publication_epoch,
    );
    let max_response_bytes =
        projection_response_limit(params.max_response_bytes, PROJECTION_EXPAND_DEFAULT_BYTES);
    Ok(Json(contain_projection_response(
        response,
        max_response_bytes,
    )))
}

fn expansion_projection(page: cih_graph_store::SubgraphPage) -> GraphProjection {
    let mut degrees = HashMap::<NodeId, u64>::new();
    let mut reduced = HashMap::<(NodeId, NodeId, EdgeKind), u64>::new();
    for edge in page.edges {
        *degrees.entry(edge.src.clone()).or_insert(0) += 1;
        *degrees.entry(edge.dst.clone()).or_insert(0) += 1;
        *reduced.entry((edge.src, edge.dst, edge.kind)).or_insert(0) += 1;
    }
    let nodes = page
        .nodes
        .into_iter()
        .map(|entry| GraphProjectionNode {
            degree: degrees.get(&entry.node.id).copied().unwrap_or(0),
            id: entry.node.id,
            kind: entry.node.kind,
            name: entry.node.name,
            role: ProjectionNodeRole::Entity,
            member_count: 1,
            expandable: entry.depth > 0,
        })
        .collect::<Vec<_>>();
    let edges = reduced
        .into_iter()
        .map(|((source, target, kind), count)| GraphProjectionEdge {
            source,
            target,
            kind,
            count,
        })
        .collect::<Vec<_>>();
    GraphProjection {
        total_nodes: nodes.len() as u64,
        total_edges: edges.len() as u64,
        truncated: page.has_more || page.traversal.truncated,
        nodes,
        edges,
    }
}

fn contain_projection_response(
    mut response: layout::LayoutProjection,
    max_response_bytes: usize,
) -> layout::LayoutProjection {
    for _ in 0..8 {
        let size = serde_json::to_vec(&response)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if size <= max_response_bytes {
            return response;
        }
        response.truncated = true;
        if response.nodes.len() <= 1 && response.edges.is_empty() {
            break;
        }
        let ratio = ((max_response_bytes as f64 / size as f64) * 0.9).clamp(0.05, 0.9);
        let keep_nodes = ((response.nodes.len() as f64 * ratio) as usize).max(1);
        let keep_edges = (response.edges.len() as f64 * ratio) as usize;
        response.nodes.truncate(keep_nodes);
        response.edges.truncate(keep_edges);
        response
            .edges
            .retain(|edge| edge.source < keep_nodes as u32 && edge.target < keep_nodes as u32);
    }
    response
}

fn projection_response_limit(raw: Option<usize>, default: usize) -> usize {
    raw.unwrap_or(default)
        .clamp(16 * 1024, PROJECTION_MAX_RESPONSE_BYTES)
}

fn parse_projection_scope(raw: Option<&str>) -> Result<ProjectionScope, BrowserError> {
    match raw
        .unwrap_or("repository")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "repository" => Ok(ProjectionScope::Repository),
        "community" => Ok(ProjectionScope::Community),
        "file" => Ok(ProjectionScope::File),
        other => Err(BrowserError::bad_request(format!(
            "unknown projection scope: {other}"
        ))),
    }
}

fn parse_projection_edge_kinds(raw: Option<&str>) -> Result<Vec<EdgeKind>, BrowserError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .filter(|value| !value.trim().is_empty())
        .map(|value| match value.trim().to_ascii_uppercase().as_str() {
            "CALLS" => Ok(EdgeKind::Calls),
            "IMPORTS" => Ok(EdgeKind::Imports),
            "EXTENDS" => Ok(EdgeKind::Extends),
            "IMPLEMENTS" => Ok(EdgeKind::Implements),
            "TESTS" => Ok(EdgeKind::Tests),
            "HANDLES_ROUTE" => Ok(EdgeKind::HandlesRoute),
            "EXTERNAL_CALL" => Ok(EdgeKind::ExternalCall),
            "PUBLISHES_EVENT" => Ok(EdgeKind::PublishesEvent),
            "LISTENS_TO" => Ok(EdgeKind::ListensTo),
            "INTEGRATION_LINK" => Ok(EdgeKind::IntegrationLink),
            "EXECUTES_QUERY" => Ok(EdgeKind::ExecutesQuery),
            "READS_TABLE" => Ok(EdgeKind::ReadsTable),
            "WRITES_TABLE" => Ok(EdgeKind::WritesTable),
            other => Err(BrowserError::bad_request(format!(
                "unsupported projection edge kind: {other}"
            ))),
        })
        .collect()
}

async fn graph_summary_handler(
    State(state): State<BrowserState>,
) -> Result<Json<GraphSummary>, BrowserError> {
    let summary = state
        .queries
        .summary()
        .await
        .map_err(BrowserError::from_app)?;
    Ok(Json(summary))
}

async fn graph_search(
    State(state): State<BrowserState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<BrowserSearchResult>, BrowserError> {
    let limit = search::query_limit(params.limit.unwrap_or(0));
    let result = state
        .queries
        .search(
            &params.q,
            limit,
            params.expand.unwrap_or(true),
            ExpansionLimits::from_wire(
                params.max_nodes.unwrap_or(0),
                params.max_edges.unwrap_or(0),
                params.max_response_bytes.unwrap_or(0),
            ),
        )
        .await
        .map_err(BrowserError::from_app)?;
    Ok(Json(result))
}

async fn graph_context(
    State(state): State<BrowserState>,
    Query(params): Query<NodeParams>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let id = node_id(params.id)?;
    let ctx = state
        .queries
        .context(&id)
        .await
        .map_err(BrowserError::from_app)?;
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
        .queries
        .impact(&id, direction, depth)
        .await
        .map_err(BrowserError::from_app)?;

    Ok(Json(render_d3_impact(&impact)))
}

async fn graph_flow(
    State(state): State<BrowserState>,
    Query(params): Query<FlowParams>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let entry_id = node_id(params.id)?;
    let depth = bounded_depth(params.depth, 6, 10);
    let flow = state
        .queries
        .flow(&entry_id, depth)
        .await
        .map_err(BrowserError::from_app)?;

    Ok(Json(render_flow_graph(
        &entry_id,
        flow.entry_node.as_ref(),
        &flow.hops,
        depth,
    )))
}

async fn graph_communities(
    State(state): State<BrowserState>,
) -> Result<Json<serde_json::Value>, BrowserError> {
    let graph = state
        .queries
        .communities()
        .await
        .map_err(BrowserError::from_app)?;
    Ok(Json(render_community_diagram(
        &graph.communities,
        &graph.edges,
    )))
}

/// Embedding clusters (feature groups) for the current repo. Reads the
/// `groups.jsonl` artifact written by `cih-engine discover --feature-strategy embed`
/// and returns clusters with their member nodes, sorted so low-confidence outliers
/// surface first — the signal for eyeballing grouping quality.
///
/// Always returns 200: when no embedding run exists it returns an empty list plus a
/// `note` explaining how to generate one, so the UI can render a friendly empty state.
///
/// [`load_feature_clusters`] is a cold artifact read (it parses the whole
/// `nodes.jsonl` to build its first-party filter), so it runs in the **heavy
/// blocking lane** rather than on a Tokio worker — at 500k nodes that is a
/// ~700 ms parse that would otherwise stall every other request on this runtime.
/// Lane saturation and the load deadline both surface as the friendly `note`.
async fn graph_features(State(state): State<BrowserState>) -> Json<serde_json::Value> {
    let artifacts_dir = state.artifacts_dir.clone();
    let loaded = run_blocking_heavy(blocking_timeout(), "graph features load", move || {
        load_feature_clusters(artifacts_dir.as_deref())
    })
    .await;
    match loaded {
        Ok(Ok(clusters)) => Json(json!({ "clusters": clusters })),
        Ok(Err(err)) => Json(json!({ "clusters": [], "note": err.to_string() })),
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
        anyhow::bail!("CIH_ARTIFACTS_DIR is not set — cannot locate embedding-cluster artifacts");
    };
    // `dir` is the artifacts root (`<repo>/.cih/artifacts`); the source graph version names the
    // sibling `<repo>/.cih/artifacts-features/<version>` that discover writes clusters into.
    let artifacts = cih_core::GraphArtifacts::latest_in_dir(dir)?;
    let version = artifacts.version.to_string();
    let repo = dir.parent().and_then(Path::parent).ok_or_else(|| {
        anyhow::anyhow!(
            "cannot derive repo root from artifacts dir {}",
            dir.display()
        )
    })?;
    let feat_dir = cih_grouping::find_feature_artifact_dir(repo, &version).ok_or_else(|| {
        #[cfg(feature = "semantic")]
        let message = format!(
            "no embedding clusters found for graph version {version} — run \
             `cih discover <repo> --feature-strategy embed` first"
        );
        #[cfg(not(feature = "semantic"))]
        let message = format!(
            "no embedding clusters found for graph version {version}; this CIH build does not \
             include semantic/vector clustering"
        );
        anyhow::anyhow!(message)
    })?;
    let mut entries = cih_grouping::read_feature_artifact(&feat_dir)?;

    // Third-party (jar/external) and test nodes are indexed only as call-graph boundary
    // stubs; the clusterer can't place them, so they flood the "shared" bucket. Drop them
    // so the cluster view shows only first-party code. Mirrors discover's `is_project_node`.
    // Best-effort: if nodes can't be read, fall back to showing everything.
    if let Ok(nodes) = artifacts.read_nodes() {
        let excluded: std::collections::HashSet<&str> = nodes
            .iter()
            .filter(|n| !is_project_node(n))
            .map(|n| n.id.as_str())
            .collect();
        entries.retain(|e| !excluded.contains(e.node_id.as_str()));
    }

    Ok(build_clusters(entries))
}

/// A first-party node eligible for feature clustering: not a third-party jar/external stub
/// and not test source. Kept in sync with `cih-engine`'s discover-side filter of the same name.
fn is_project_node(n: &Node) -> bool {
    let is_external = n
        .props
        .as_ref()
        .map(|p| {
            p.get("external").and_then(|v| v.as_bool()).unwrap_or(false)
                || p.get("fromJar").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .unwrap_or(false);
    let f = n.file.as_str();
    let is_test = f.ends_with(".jar")
        || f.contains("src/test/")
        || f.contains("/test/java/")
        || f.contains("/test/kotlin/");
    !is_external && !is_test
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
        .queries
        .routes(prefix, limit)
        .await
        .map_err(BrowserError::from_app)?;
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

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }

    fn from_app(error: AppError) -> Self {
        match error {
            AppError::InvalidInput { field, message } => {
                Self::bad_request(format!("invalid {field}: {message}"))
            }
            AppError::NotFound { entity, key } => Self {
                status: StatusCode::NOT_FOUND,
                message: format!("{entity} not found: {key}"),
            },
            AppError::Unavailable { message, .. } => Self::internal(message),
            AppError::GraphUnavailable { code, message, .. } => {
                Self::unavailable(format!("{code}: {message}"))
            }
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
    use std::sync::Arc;

    use crate::domain::repository::{RepoCatalogSnapshot, RepoSelector, ResolvedRepo};
    use crate::ports::repo_context_provider::{RepoContext, RepoContextProvider};

    struct UnexpectedRepoContexts;

    #[async_trait::async_trait]
    impl RepoContextProvider for UnexpectedRepoContexts {
        fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
            panic!("blank browser search must not inspect repository state")
        }

        fn resolve_repo(&self, _selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
            panic!("blank browser search must not resolve repository identity")
        }

        async fn resolve(&self, _selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
            panic!("blank browser search must not initialize repository infrastructure")
        }
    }

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

    #[test]
    fn projection_wire_bounds_and_edge_kinds_are_strict() {
        assert_eq!(
            parse_projection_scope(Some("community")).unwrap(),
            ProjectionScope::Community
        );
        assert!(parse_projection_scope(Some("unbounded")).is_err());
        assert_eq!(
            parse_projection_edge_kinds(Some("calls,imports")).unwrap(),
            vec![EdgeKind::Calls, EdgeKind::Imports]
        );
        assert!(parse_projection_edge_kinds(Some("member_of")).is_err());
        assert_eq!(
            limit_or_default(
                Some(usize::MAX),
                2_000,
                cih_graph_store::PROJECTION_NODE_LIMIT
            ),
            cih_graph_store::PROJECTION_NODE_LIMIT
        );
    }

    #[test]
    fn projection_response_is_contained_to_wire_budget() {
        let nodes = (0..10_000)
            .map(|index| GraphProjectionNode {
                id: NodeId::new(format!("Function:fixture_{index}")),
                kind: cih_core::NodeKind::Function,
                name: format!("fixture_{index}"),
                role: ProjectionNodeRole::Entity,
                member_count: 1,
                degree: 10,
                expandable: true,
            })
            .collect::<Vec<_>>();
        let edges = (0..50_000)
            .map(|index| GraphProjectionEdge {
                source: NodeId::new(format!("Function:fixture_{}", index % 10_000)),
                target: NodeId::new(format!("Function:fixture_{}", (index * 17 + 23) % 10_000)),
                kind: EdgeKind::Calls,
                count: 1,
            })
            .collect::<Vec<_>>();
        let response = layout::compute_projection(
            GraphProjection {
                nodes,
                edges,
                total_nodes: 400_000,
                total_edges: 1_200_000,
                truncated: true,
            },
            ProjectionScope::Repository,
            None,
            Some("epoch".into()),
        );
        let contained = contain_projection_response(response, PROJECTION_MAX_RESPONSE_BYTES);
        assert!(serde_json::to_vec(&contained).unwrap().len() <= PROJECTION_MAX_RESPONSE_BYTES);
        assert!(contained.truncated);
        assert!(contained
            .edges
            .iter()
            .all(|edge| edge.source < contained.nodes.len() as u32
                && edge.target < contained.nodes.len() as u32));
    }

    #[tokio::test]
    async fn browser_handler_maps_application_validation_to_bad_request() {
        let state = BrowserState::new(
            GraphBrowserService::new(Arc::new(UnexpectedRepoContexts)),
            None,
        );

        let error = graph_search(
            State(state),
            Query(SearchParams {
                q: " ".into(),
                limit: None,
                expand: None,
                max_nodes: None,
                max_edges: None,
                max_response_bytes: None,
            }),
        )
        .await
        .expect_err("blank query must fail in the application service");

        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }
}
