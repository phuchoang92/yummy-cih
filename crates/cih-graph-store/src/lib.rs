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
mod traversal;

/// Maximum number of source nodes sent to an adapter's one-hop query.
pub const EXECUTION_BATCH_SIZE: usize = 256;
/// Hard bound for a shared execution walk.
pub const TRAVERSAL_NODE_BUDGET: usize = 10_000;
/// Hard bound for relationships examined by a shared execution walk.
pub const TRAVERSAL_EDGE_BUDGET: usize = 50_000;
/// Largest offset + page-size window accepted by `trace_flow`.
pub const FLOW_VISIBLE_WINDOW: usize = 5_000;

#[cfg(feature = "test-support")]
pub mod contract;

#[derive(thiserror::Error, Debug)]
pub enum GraphStoreError {
    #[error("graph backend error: {0}")]
    Backend(String),
    #[error("node not found: {0}")]
    NotFound(String),
    #[error("not implemented for this backend: {0}")]
    Unimplemented(&'static str),
    #[error("invalid graph query: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, GraphStoreError>;

fn is_false(value: &bool) -> bool {
    !*value
}

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

/// One edge along a [`PathInfo`], with resolution provenance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathEdge {
    /// Edge kind label (`CALLS`, `EXECUTES_QUERY`, `WRITES_TABLE`, …).
    pub kind: String,
    pub confidence: f32,
    /// Resolution reason (`di-qualifier`, `receiver-bound`, `sql-scan`, …).
    pub reason: String,
    /// True when logical execution walks opposite the stored relationship
    /// direction (Route→handler and topic→listener).
    #[serde(default, skip_serializing_if = "is_false")]
    pub traversed_reverse: bool,
}

/// A start→target path across call and side-effect edges, answering
/// "does X reach Y?" with evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathInfo {
    /// Every node along the path, endpoints included.
    pub nodes: Vec<NodeId>,
    pub edges: Vec<PathEdge>,
    /// The weakest edge's confidence — the path is only as trustworthy as that.
    pub min_confidence: f32,
}

/// Optional database-access constraint for a path's final edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathAccess {
    #[default]
    Any,
    Read,
    Write,
}

/// Bounds and semantics for [`GraphStore::paths_between`].
#[derive(Clone, Debug)]
pub struct PathFilter {
    pub max_depth: u32,
    pub max_paths: usize,
    pub access: PathAccess,
}

/// Shared traversal accounting. `truncated` means the answer is incomplete,
/// not that the target is unreachable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TraversalStats {
    pub visited_nodes: usize,
    pub expanded_edges: usize,
    pub truncated: bool,
}

/// Shortest path results plus honest budget/path-cap metadata.
#[derive(Clone, Debug)]
pub struct PathPage {
    pub paths: Vec<PathInfo>,
    pub has_more: bool,
    pub traversal: TraversalStats,
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
    /// Spring AOP advice methods whose pointcut matches this hop (`ADVISES`
    /// edges into it). Advice is not a call-graph hop — the proxy wraps the
    /// call invisibly — so it annotates the node instead of extending the path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intercepted_by: Vec<InterceptingAdvice>,
}

/// One aspect advice intercepting a traced method.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InterceptingAdvice {
    /// The advice method node (e.g. `Method:com.acme.LoggingAspect#log/1`).
    pub advice: NodeId,
    /// `around` / `before` / `after` / `after_returning` / `after_throwing`.
    pub advice_kind: String,
}

/// A database effect of a traced method: the DbQuery it executes and one table
/// that query touches. A method executing two queries (or one query touching two
/// tables) yields one entry per (query, table) pair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DbEffect {
    /// The method/constructor executing the query.
    pub method: NodeId,
    /// The DbQuery node.
    pub query: NodeId,
    /// SQL operation (`SELECT`/`INSERT`/`UPDATE`/`DELETE`/…, `UNKNOWN` when undetected).
    pub operation: String,
    /// Table name.
    pub table: String,
    /// `READ` or `WRITE`.
    pub access: String,
    /// Truncated SQL text for display.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sql_preview: String,
}

impl DbEffect {
    /// Build from a raw adapter row: the DbQuery's serialized `props` JSON (source
    /// of `operation`/`sqlPreview` — they are not promoted graph properties) and
    /// the relationship label (`WRITES_TABLE` / `READS_TABLE`).
    pub fn from_query_row(
        method: NodeId,
        query: NodeId,
        props_json: &str,
        table: String,
        rel_label: &str,
    ) -> Self {
        let props: serde_json::Value =
            serde_json::from_str(props_json).unwrap_or(serde_json::Value::Null);
        let str_prop = |key: &str, default: &str| {
            props
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or(default)
                .to_string()
        };
        Self {
            method,
            query,
            operation: str_prop("operation", "UNKNOWN"),
            sql_preview: str_prop("sqlPreview", ""),
            table,
            access: if rel_label == "WRITES_TABLE" {
                "WRITE"
            } else {
                "READ"
            }
            .to_string(),
        }
    }
}

/// Node and paging filter for [`GraphStore::flow_downstream`].
#[derive(Clone, Debug, Default)]
pub struct FlowFilter {
    /// Maximum traversal depth (the shared walk clamps to 1..=10).
    pub max_depth: u32,
    /// Node kinds hidden from the reported hops. Paths still traverse hidden
    /// nodes, so a visible hop may name a hidden parent.
    pub exclude_kinds: Vec<NodeKind>,
    /// Hide trivial accessors (`isAccessor` promoted prop). No-op on graphs
    /// loaded before the prop existed.
    pub exclude_accessors: bool,
    /// Page size; 0 = default (100).
    pub limit: usize,
    /// Hops to skip — the continuation offset from a prior page.
    pub offset: usize,
}

impl FlowFilter {
    /// Depth-only filter: everything included, first page, default size.
    pub fn depth(max_depth: u32) -> Self {
        Self {
            max_depth,
            ..Self::default()
        }
    }

    /// Effective page size (0 → 100).
    pub fn effective_limit(&self) -> usize {
        if self.limit == 0 {
            100
        } else {
            self.limit
        }
    }
}

/// One page of a trace_flow walk.
#[derive(Clone, Debug)]
pub struct FlowPage {
    pub hops: Vec<FlowHop>,
    /// True when hops beyond this page exist (fetch the next page via
    /// `offset + hops.len()`).
    pub has_more: bool,
    pub traversal: TraversalStats,
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

/// One logical execution transition returned by a backend's batched one-hop
/// primitive. The target carries enough metadata for the shared BFS to avoid a
/// per-node lookup.
#[derive(Clone, Debug)]
pub struct ExecutionTransition {
    pub source: NodeId,
    pub target: Node,
    pub kind: String,
    pub confidence: f32,
    pub reason: String,
    pub call_sites: Vec<CallSiteArgs>,
    pub traversed_reverse: bool,
    pub target_is_accessor: bool,
}

/// AOP advice attached to one traced method by a batched adapter lookup.
#[derive(Clone, Debug)]
pub struct Interception {
    pub target: NodeId,
    pub advice: InterceptingAdvice,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KindCount {
    pub kind: String,
    pub count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphSummary {
    pub kinds: Vec<KindCount>,
    pub total_nodes: u64,
    pub total_edges: u64,
}

/// The pluggable storage port. MCP tools map 1:1 onto the read methods.
#[async_trait]
pub trait GraphStore: Send + Sync {
    // ---- writes / lifecycle ----
    async fn ensure_schema(&self) -> Result<()>;
    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats>;
    async fn upsert_incremental(&self, delta: &GraphDelta) -> Result<()>;
    /// Copy this store's graph into `dest_key`, replacing the destination atomically.
    ///
    /// Port guarantee: after `publish_to` returns, dropping this (source/staging)
    /// graph must not affect the published data — the engine drops staging right
    /// after publishing, so publish may not alias storage with the source.
    async fn publish_to(&self, dest_key: &str) -> Result<()>;
    /// Delete this store's graph entirely. Idempotent: succeeds when the graph
    /// does not exist.
    async fn drop_graph(&self) -> Result<()>;
    /// Bulk load with phase callbacks. The default ignores the observer and
    /// delegates to [`bulk_load`](GraphStore::bulk_load), so adapters without
    /// phase events implement nothing extra.
    async fn bulk_load_observed(
        &self,
        artifacts: &GraphArtifacts,
        obs: &dyn LoadObserver,
    ) -> Result<LoadStats> {
        let _ = obs;
        self.bulk_load(artifacts).await
    }

    // ---- reads (domain queries) ----
    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>>;
    async fn neighbors(&self, id: &NodeId, dir: Direction, kinds: &[EdgeKind])
        -> Result<Vec<Edge>>;
    async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact>;
    async fn call_chain(&self, from: &NodeId, to: &NodeId, max_depth: u32) -> Result<Vec<Path>>;
    async fn subgraph(&self, seeds: &[NodeId], radius: u32) -> Result<Subgraph>;
    /// Return per-kind node counts and total graph size. Fast — no degree scan.
    async fn graph_summary(&self) -> Result<GraphSummary>;
    /// Return a deterministic, bounded whole-graph projection for interactive
    /// visualization. When `kinds` is `Some`, only nodes of those kinds are included.
    /// When `None`, implementations prioritize architectural nodes then high-degree symbols.
    async fn graph_overview(
        &self,
        max_nodes: usize,
        max_edges: usize,
        kinds: Option<&[String]>,
    ) -> Result<GraphOverview>;
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

    /// Return logical one-hop execution transitions for at most
    /// [`EXECUTION_BATCH_SIZE`] source ids. Adapters preserve stored edge
    /// orientation in the graph but return Route→handler and topic→listener as
    /// reverse logical transitions.
    async fn execution_transitions(
        &self,
        _ids: &[NodeId],
        _include_data: bool,
        _limit: usize,
    ) -> Result<Vec<ExecutionTransition>> {
        Err(GraphStoreError::Unimplemented("execution_transitions"))
    }

    /// Return Spring AOP advice for a batch of traced methods. Backends without
    /// promoted ADVISES support may return an empty list.
    async fn interceptions_for_methods(&self, _ids: &[NodeId]) -> Result<Vec<Interception>> {
        Ok(Vec::new())
    }

    /// Trace the downstream execution chain using the backend-neutral bounded
    /// BFS. Node filters hide results but never sever traversal paths.
    async fn flow_downstream(&self, entry: &NodeId, filter: &FlowFilter) -> Result<FlowPage> {
        traversal::flow_downstream(self, entry, filter).await
    }

    /// Database effects of the given methods: every `(method)-[:EXECUTES_QUERY]->
    /// (DbQuery)-[:READS_TABLE|WRITES_TABLE]->(DbTable)` chain, one entry per
    /// (method, query, table). Used to surface table reads/writes on traces.
    async fn db_effects_for_methods(&self, ids: &[NodeId]) -> Result<Vec<DbEffect>>;

    /// Shortest logical execution paths from `from` to `to`, with per-edge
    /// provenance and honest traversal/path caps.
    async fn paths_between(
        &self,
        from: &NodeId,
        to: &NodeId,
        filter: &PathFilter,
    ) -> Result<PathPage> {
        traversal::paths_between(self, from, to, filter).await
    }

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

/// Coarse load-phase callbacks so a CLI can render multi-phase progress while a
/// bulk load runs. Every method defaults to a no-op, so an adapter only fires the
/// phases it actually has and a caller only overrides the ones it displays. The
/// FalkorDB adapter fires `nodes_loaded`/`edges_loaded`/`indexes_built` from
/// inside the bulk insert; connect/staging/publish are engine-orchestrated.
///
/// `Send + Sync` (not `Send`-future) is sufficient: the load runs on a
/// current-thread runtime, and a `&dyn LoadObserver` is `Send` because the trait
/// is `Sync`.
pub trait LoadObserver: Send + Sync {
    fn nodes_loaded(&self, _count: u64) {}
    fn edges_loaded(&self, _count: u64) {}
    fn indexes_built(&self) {}
}

/// A `LoadObserver` that ignores every event — the default when no progress
/// display is wanted (tests, `discover`/`taint`/`artifact`, non-observed loads).
pub struct NoopObserver;
impl LoadObserver for NoopObserver {}

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

#[cfg(test)]
mod tests {
    use super::risk_from_fanout;

    #[test]
    fn risk_from_fanout_boundaries() {
        // Exact bucket edges — guards the thresholds every adapter shares.
        assert_eq!(risk_from_fanout(0), "none");
        assert_eq!(risk_from_fanout(1), "low");
        assert_eq!(risk_from_fanout(5), "low");
        assert_eq!(risk_from_fanout(6), "medium");
        assert_eq!(risk_from_fanout(20), "medium");
        assert_eq!(risk_from_fanout(21), "high");
        assert_eq!(risk_from_fanout(75), "high");
        assert_eq!(risk_from_fanout(76), "critical");
        assert_eq!(risk_from_fanout(usize::MAX), "critical");
    }
}
