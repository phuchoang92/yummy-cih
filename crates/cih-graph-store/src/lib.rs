//! `GraphStore` — the storage-agnostic port. The engine and MCP tools talk
//! ONLY to this trait; each graph DB is an adapter (cih-falkor, future
//! cih-neptune, cih-postgres). Methods are DOMAIN operations, not raw queries,
//! so swapping backends never touches callers.
//!
//! Neptune / Neo4j / FalkorDB all speak openCypher → they share a
//! `CypherGraphStore` impl (parameterized by a driver + dialect); only the
//! Postgres-CTE adapter is fully separate.

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind};
use serde::{Deserialize, Serialize};

#[derive(thiserror::Error, Debug)]
pub enum GraphStoreError {
    #[error("graph backend error: {0}")]
    Backend(String),
    #[error("node not found: {0}")]
    NotFound(String),
    #[error("not implemented for this backend: {0}")]
    Unimplemented(&'static str),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, GraphStoreError>;

/// Traversal direction for impact / neighbor queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// callers — who depends on this symbol (blast radius).
    Upstream,
    /// callees — what this symbol depends on.
    Downstream,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadStats {
    pub nodes: u64,
    pub edges: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImpactNode {
    pub id: NodeId,
    pub depth: u32,
    pub via: String,
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<NodeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Impact {
    pub root: NodeId,
    pub direction: Direction,
    pub affected: Vec<ImpactNode>,
    /// none | low | medium | high | critical (derived from fan-out).
    pub risk: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Path {
    pub nodes: Vec<NodeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// A bounded, read-only projection used by whole-repository graph explorers.
///
/// `degree` is the undirected degree in the complete stored graph, not only in
/// the returned projection. This lets clients preserve visually important hubs
/// even when the overview is sampled.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphOverviewNode {
    pub node: Node,
    pub degree: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphOverviewEdge {
    pub source: NodeId,
    pub target: NodeId,
    pub kind: EdgeKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphOverview {
    pub nodes: Vec<GraphOverviewNode>,
    pub edges: Vec<GraphOverviewEdge>,
    pub total_nodes: u64,
    pub total_edges: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolContext {
    pub node: Node,
    pub callers: Vec<Node>,
    pub callees: Vec<Node>,
    pub processes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community: Option<CommunityInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityInfo {
    pub id: String,
    pub name: String,
    pub symbol_count: u64,
    pub cohesion: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: Option<String>,
    pub file: String,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<NodeId>,
}

/// One step in a trace_flow result: the symbol reached, and the edge used to reach it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowHop {
    pub node: FlowNode,
    /// None for the root entry point.
    pub via: Option<FlowEdge>,
}

/// The edge connecting two hops in a trace_flow result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowEdge {
    /// Edge kind label, e.g. "CALLS", "HANDLES_ROUTE".
    pub kind: String,
    /// Call-site argument records from the edge's `callSites` property.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_sites: Vec<CallSiteArgs>,
}

/// Argument texts captured at one call site.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallSiteArgs {
    /// Resolved (constant-propagated) argument expressions.
    pub args: Vec<String>,
}

/// A method node returned by complexity_hotspots.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotspotNode {
    pub id: NodeId,
    pub name: String,
    pub file: String,
    pub cyclomatic: u64,
    pub cognitive: u64,
    pub transitive_loop_depth: u64,
}

/// A near-duplicate method candidate returned by similar_methods.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimilarMethod {
    pub id: NodeId,
    pub name: String,
    pub file: String,
    pub jaccard: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunityEdge {
    pub src: String,
    pub dst: String,
    pub weight: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteInfo {
    pub path: String,
    pub http_method: String,
    pub decorator: String,
    pub handler_id: NodeId,
    pub handler_name: String,
    pub handler_qualified: String,
}

/// The pluggable storage port. MCP tools map 1:1 onto the read methods.
#[async_trait]
pub trait GraphStore: Send + Sync {
    // ---- writes / lifecycle ----
    async fn ensure_schema(&self) -> Result<()>;
    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats>;
    async fn upsert_incremental(&self, delta: &GraphDelta) -> Result<()>;
    /// Copy this store's graph into `dest_key`, replacing the destination atomically.
    async fn publish_to(&self, dest_key: &str) -> Result<()>;

    // ---- reads (domain queries) ----
    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>>;
    async fn neighbors(&self, id: &NodeId, dir: Direction, kinds: &[EdgeKind])
        -> Result<Vec<Edge>>;
    async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact>;
    async fn call_chain(&self, from: &NodeId, to: &NodeId, max_depth: u32) -> Result<Vec<Path>>;
    async fn subgraph(&self, seeds: &[NodeId], radius: u32) -> Result<Subgraph>;
    /// Return a deterministic, bounded whole-graph projection for interactive
    /// visualization. Implementations should prioritize architectural nodes and
    /// high-degree symbols, then include only edges whose endpoints were kept.
    async fn graph_overview(&self, max_nodes: usize, max_edges: usize) -> Result<GraphOverview>;
    async fn context(&self, id: &NodeId) -> Result<SymbolContext>;
    async fn communities(&self) -> Result<Vec<CommunityInfo>>;
    async fn route_map(&self, prefix: Option<&str>, limit: usize) -> Result<Vec<RouteInfo>>;

    // ---- Phase 19: disambiguation + change detection ----

    /// Find all nodes whose simple `name` property matches exactly (case-sensitive).
    /// Returns up to `limit` candidates. Used for ambiguous-symbol detection when
    /// the caller supplies a short name without a kind prefix.
    async fn candidates_by_name(&self, name: &str, limit: usize) -> Result<Vec<Node>>;

    /// Find all nodes whose `file` property is in `files` (repo-relative paths).
    /// Scoped to callable/structural kinds (Method, Constructor, Function, Class,
    /// Interface, Enum). Used by `detect_changes` to map changed files → symbols.
    async fn nodes_in_files(&self, files: &[String]) -> Result<Vec<Node>>;

    /// Return the Process node IDs directly reachable from `symbol_ids` via
    /// STEP_IN_PROCESS edges.  Used by `detect_changes` to list affected processes.
    async fn processes_for_symbols(&self, symbol_ids: &[NodeId]) -> Result<Vec<String>>;

    /// Trace the downstream execution chain from an entry point.
    /// Traverses CALLS, HANDLES_ROUTE, EXTERNAL_CALL, PUBLISHES_EVENT, LISTENS_TO edges.
    /// Returns all reachable hops ordered by minimum depth, capped at 100.
    /// Each hop carries the edge used to reach it (with call-site args if available).
    async fn flow_downstream(&self, entry: &NodeId, max_depth: u32) -> Result<Vec<FlowHop>>;

    /// Return methods with complexity above the given thresholds (Gap 1).
    /// `min_transitive_loop` defaults to 1 if None.
    async fn complexity_hotspots(
        &self,
        min_cyclomatic: Option<u16>,
        min_cognitive: Option<u16>,
        min_transitive_loop: Option<u8>,
        limit: usize,
    ) -> Result<Vec<HotspotNode>>;

    /// Return near-duplicate methods of `id` with Jaccard >= `min_jaccard` (Gap 2).
    async fn similar_methods(
        &self,
        id: &NodeId,
        min_jaccard: f32,
        limit: usize,
    ) -> Result<Vec<SimilarMethod>>;

    /// Return the community each node belongs to (via MEMBER_OF edges).
    /// Nodes with no community are omitted from the result.
    async fn symbol_communities(&self, ids: &[NodeId]) -> Result<Vec<(NodeId, CommunityInfo)>>;

    /// Return all test method/class nodes that have a direct TESTS edge to `id` or
    /// to the class that owns `id`. Returns up to 50 results.
    async fn test_coverage(&self, id: &NodeId) -> Result<Vec<Node>>;

    /// Given repo-relative file paths, return the distinct test class/method nodes
    /// that have a TESTS edge to any symbol in those files.
    async fn tests_for_files(&self, files: &[String]) -> Result<Vec<Node>>;

    /// Return production symbols (Method, Class, Interface) under `file_prefix`
    /// that have no inbound TESTS edge — i.e. no known test coverage.
    async fn untested_symbols(&self, file_prefix: &str, limit: usize) -> Result<Vec<Node>>;

    /// Return inter-community CALLS edges: for each pair of communities (A, B),
    /// the number of CALLS edges from a member of A to a member of B. Used to
    /// render the community service-map diagram. Returns empty if no discover run
    /// has been done (no Community nodes in graph).
    async fn community_graph(&self) -> Result<Vec<CommunityEdge>>;
}

/// Bulk loading is a SEPARATE port — mechanisms differ wildly across backends
/// (Neptune S3 loader, Neo4j admin import, FalkorDB bulk tool, Postgres COPY).
#[async_trait]
pub trait BulkLoader: Send + Sync {
    async fn load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats>;
}

/// Derive a coarse risk label from upstream fan-out — shared helper so every
/// adapter reports risk consistently.
pub fn risk_from_fanout(affected: usize) -> &'static str {
    match affected {
        0 => "none",
        1..=5 => "low",
        6..=20 => "medium",
        21..=75 => "high",
        _ => "critical",
    }
}
