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
/// Compatibility/default page size for each independent context section.
pub const CONTEXT_DEFAULT_LIMIT: usize = 100;
/// Hard cap for callers, callees, and processes in one context page.
pub const CONTEXT_MAX_LIMIT: usize = 500;
/// Temporary deterministic containment cap for backend-recursive impact reads.
pub const IMPACT_RESULT_LIMIT: usize = 200;
/// Largest page requested from a backend one-hop transition query. The
/// adapters probe with one extra row, keeping the request below FalkorDB's
/// verified 10,000-row result cap.
pub const TRANSITION_PAGE_MAX: usize = 8_000;
/// Compatibility node bound for the legacy `subgraph` read.
pub const SUBGRAPH_NODE_LIMIT: usize = 200;
/// Compatibility edge bound for the legacy `subgraph` read.
pub const SUBGRAPH_EDGE_LIMIT: usize = 1_000;

#[cfg(feature = "test-support")]
pub mod contract;

/// Machine-readable retry guidance carried by operational graph-store errors.
///
/// `retry_after_ms` is advisory. Callers should still apply their own retry
/// budget and jitter rather than retrying forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryMetadata {
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl RetryMetadata {
    pub const fn retryable(retry_after_ms: Option<u64>) -> Self {
        Self {
            retryable: true,
            retry_after_ms,
        }
    }

    pub const fn non_retryable() -> Self {
        Self {
            retryable: false,
            retry_after_ms: None,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum GraphStoreError {
    #[error("graph backend is loading: {message}")]
    Loading {
        message: String,
        retry: RetryMetadata,
    },
    #[error("graph backend is overloaded: {message}")]
    Overloaded {
        message: String,
        retry: RetryMetadata,
    },
    #[error("graph query exceeded its {timeout_ms}ms execution deadline: {message}")]
    ExecutionTimeout {
        message: String,
        timeout_ms: u64,
        retry: RetryMetadata,
    },
    #[error("graph backend is unavailable: {message}")]
    Unavailable {
        message: String,
        retry: RetryMetadata,
    },
    #[error("graph index operation failed: {message}")]
    Index {
        message: String,
        retry: RetryMetadata,
    },
    /// Compatibility escape hatch for adapters that have not classified an
    /// operational backend failure yet.
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

impl GraphStoreError {
    /// Retry metadata for classified operational errors. Domain/input errors
    /// and the legacy unclassified [`Self::Backend`] variant return `None`.
    pub const fn retry_metadata(&self) -> Option<RetryMetadata> {
        match self {
            Self::Loading { retry, .. }
            | Self::Overloaded { retry, .. }
            | Self::ExecutionTimeout { retry, .. }
            | Self::Unavailable { retry, .. }
            | Self::Index { retry, .. } => Some(*retry),
            Self::Backend(_)
            | Self::NotFound(_)
            | Self::Unimplemented(_)
            | Self::InvalidInput(_)
            | Self::Other(_) => None,
        }
    }
}

#[cfg(test)]
mod error_tests {
    use super::{BackendReadiness, BackendReadinessState, GraphStoreError, RetryMetadata};

    #[test]
    fn backend_readiness_constructors_preserve_restore_guidance() {
        assert_eq!(
            BackendReadiness::ready().state,
            BackendReadinessState::Ready
        );

        let loading = BackendReadiness::loading("restore in progress", 1_000);
        assert_eq!(loading.state, BackendReadinessState::Loading);
        assert_eq!(loading.detail.as_deref(), Some("restore in progress"));
        assert_eq!(loading.retry_after_ms, Some(1_000));
    }

    #[test]
    fn classified_operational_errors_expose_retry_metadata() {
        let retry = RetryMetadata::retryable(Some(250));
        let error = GraphStoreError::Loading {
            message: "dataset restore".into(),
            retry,
        };
        assert_eq!(error.retry_metadata(), Some(retry));

        let terminal = GraphStoreError::ExecutionTimeout {
            message: "bounded query".into(),
            timeout_ms: 500,
            retry: RetryMetadata::non_retryable(),
        };
        assert_eq!(
            terminal.retry_metadata(),
            Some(RetryMetadata::non_retryable())
        );
    }

    #[test]
    fn legacy_backend_error_remains_available_but_unclassified() {
        let error = GraphStoreError::Backend("adapter detail".into());
        assert_eq!(error.to_string(), "graph backend error: adapter detail");
        assert_eq!(error.retry_metadata(), None);
    }
}

pub type Result<T> = std::result::Result<T, GraphStoreError>;

/// Backend-level serving state, independent of any repository publication.
///
/// Remote adapters should override [`GraphStore::backend_readiness`] with a
/// bounded, read-only metadata command. Embedded adapters may use the default
/// ready state because successful construction already proves that the local
/// backend is available.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendReadinessState {
    Ready,
    Loading,
}

/// Result of a non-mutating backend readiness probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendReadiness {
    pub state: BackendReadinessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl BackendReadiness {
    pub const fn ready() -> Self {
        Self {
            state: BackendReadinessState::Ready,
            detail: None,
            retry_after_ms: None,
        }
    }

    pub fn loading(detail: impl Into<String>, retry_after_ms: u64) -> Self {
        Self {
            state: BackendReadinessState::Loading,
            detail: Some(detail.into()),
            retry_after_ms: Some(retry_after_ms),
        }
    }
}

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
    /// True when another affected node was observed by the `limit + 1` probe.
    #[serde(default)]
    pub has_more: bool,
    /// Maximum affected nodes returned by this compatibility query.
    #[serde(default)]
    pub backend_limit: usize,
    /// Observable traversal accounting. Recursive backend queries cannot expose
    /// expanded-edge counts yet, so that field remains zero until shared BFS.
    #[serde(default)]
    pub traversal: TraversalStats,
    /// Whether `risk` was calculated from the complete requested depth scope.
    #[serde(default)]
    pub risk_exact: bool,
    /// Explicit compatibility signal that `risk` is only a lower bound.
    #[serde(default)]
    pub risk_lower_bound: bool,
}

/// Outcome of a bounded graph walk. `Truncated` means the requested result
/// page was bounded after the full scope was evaluated. `Inconclusive` means
/// a work/backend budget prevented evaluation of the complete scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraversalStatus {
    #[default]
    Complete,
    Truncated,
    Inconclusive,
}

/// Machine-readable reason a bounded traversal did not return a complete
/// result set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraversalReason {
    ResultLimit,
    NodeBudget,
    EdgeBudget,
    BackendLimit,
}

/// Bounds for the shared CALLS impact BFS.
#[derive(Clone, Debug)]
pub struct ImpactFilter {
    pub direction: Direction,
    pub max_depth: u32,
    pub result_limit: usize,
    pub node_budget: usize,
    pub edge_budget: usize,
    pub transition_page_limit: usize,
}

impl ImpactFilter {
    /// Compatibility bounds used by the legacy [`GraphStore::impact`] method.
    pub fn compatibility(direction: Direction, max_depth: u32) -> Self {
        Self {
            direction,
            max_depth: max_depth.clamp(1, 20),
            result_limit: IMPACT_RESULT_LIMIT,
            node_budget: TRAVERSAL_NODE_BUDGET,
            edge_budget: TRAVERSAL_EDGE_BUDGET,
            transition_page_limit: TRANSITION_PAGE_MAX,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=20).contains(&self.max_depth) {
            return Err(GraphStoreError::InvalidInput(
                "impact max_depth must be in 1..=20".to_string(),
            ));
        }
        if self.result_limit == 0 || self.result_limit > TRAVERSAL_NODE_BUDGET {
            return Err(GraphStoreError::InvalidInput(format!(
                "impact result_limit must be in 1..={TRAVERSAL_NODE_BUDGET}"
            )));
        }
        if self.node_budget == 0 || self.node_budget > TRAVERSAL_NODE_BUDGET {
            return Err(GraphStoreError::InvalidInput(format!(
                "impact node_budget must be in 1..={TRAVERSAL_NODE_BUDGET}"
            )));
        }
        if self.edge_budget == 0 || self.edge_budget > TRAVERSAL_EDGE_BUDGET {
            return Err(GraphStoreError::InvalidInput(format!(
                "impact edge_budget must be in 1..={TRAVERSAL_EDGE_BUDGET}"
            )));
        }
        validate_transition_page_limit(self.transition_page_limit)
    }
}

/// Truthful metadata around the legacy impact payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImpactPage {
    pub impact: Impact,
    pub status: TraversalStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<TraversalReason>,
    /// Number of affected nodes observed before result-page truncation. This
    /// is an exact total only when `impact.risk_exact` is true.
    pub evaluated_affected: usize,
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

/// One node selected by the shared multi-root stored-orientation walk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubgraphNode {
    pub node: Node,
    pub depth: u32,
    pub root: bool,
}

/// Bounds for [`GraphStore::subgraph_bounded`].
#[derive(Clone, Debug)]
pub struct SubgraphFilter {
    pub radius: u32,
    pub node_limit: usize,
    pub edge_limit: usize,
    pub transition_page_limit: usize,
}

impl SubgraphFilter {
    pub fn compatibility(radius: u32) -> Self {
        Self {
            radius: radius.clamp(1, 4),
            node_limit: SUBGRAPH_NODE_LIMIT,
            edge_limit: SUBGRAPH_EDGE_LIMIT,
            transition_page_limit: TRANSITION_PAGE_MAX,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.radius > 20 {
            return Err(GraphStoreError::InvalidInput(
                "subgraph radius must be in 0..=20".to_string(),
            ));
        }
        if self.node_limit == 0 || self.node_limit > TRAVERSAL_NODE_BUDGET {
            return Err(GraphStoreError::InvalidInput(format!(
                "subgraph node_limit must be in 1..={TRAVERSAL_NODE_BUDGET}"
            )));
        }
        if self.edge_limit == 0 || self.edge_limit > TRAVERSAL_EDGE_BUDGET {
            return Err(GraphStoreError::InvalidInput(format!(
                "subgraph edge_limit must be in 1..={TRAVERSAL_EDGE_BUDGET}"
            )));
        }
        validate_transition_page_limit(self.transition_page_limit)
    }
}

/// Result of one bounded multi-root stored-orientation expansion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubgraphPage {
    pub roots: Vec<NodeId>,
    pub nodes: Vec<SubgraphNode>,
    pub edges: Vec<Edge>,
    pub status: TraversalStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<TraversalReason>,
    pub has_more: bool,
    pub traversal: TraversalStats,
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

/// Stable key used by graph adapters to continue one independently paged
/// context section. Transports should wrap this in an opaque operation-specific
/// cursor rather than exposing the fields as a public wire contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCursorKey {
    pub name: String,
    pub id: String,
}

/// Independent bounds and continuation state for callers, callees, and
/// processes. Each limit must be in `1..=CONTEXT_MAX_LIMIT`.
#[derive(Clone, Debug)]
pub struct ContextFilter {
    pub caller_limit: usize,
    pub callee_limit: usize,
    pub process_limit: usize,
    pub caller_after: Option<ContextCursorKey>,
    pub callee_after: Option<ContextCursorKey>,
    pub process_after: Option<ContextCursorKey>,
}

impl Default for ContextFilter {
    fn default() -> Self {
        Self {
            caller_limit: CONTEXT_DEFAULT_LIMIT,
            callee_limit: CONTEXT_DEFAULT_LIMIT,
            process_limit: CONTEXT_DEFAULT_LIMIT,
            caller_after: None,
            callee_after: None,
            process_after: None,
        }
    }
}

impl ContextFilter {
    pub fn validate(&self) -> Result<()> {
        for (section, limit) in [
            ("caller", self.caller_limit),
            ("callee", self.callee_limit),
            ("process", self.process_limit),
        ] {
            if !(1..=CONTEXT_MAX_LIMIT).contains(&limit) {
                return Err(GraphStoreError::InvalidInput(format!(
                    "{section} context limit {limit} is outside 1..={CONTEXT_MAX_LIMIT}"
                )));
            }
        }
        Ok(())
    }
}

/// One independently bounded section of a context result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextSection<T> {
    pub items: Vec<T>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<ContextCursorKey>,
}

/// Bounded 360-degree symbol context. Each section advances with its own key so
/// a high-degree caller set cannot consume the callee or process budget.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextPage {
    pub node: Node,
    pub callers: ContextSection<Node>,
    pub callees: ContextSection<Node>,
    pub processes: ContextSection<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community: Option<CommunityInfo>,
}

impl ContextSection<Node> {
    /// Build a page from an already stable or unsorted `limit + 1` node probe.
    pub fn from_node_probe(
        mut nodes: Vec<Node>,
        limit: usize,
        after: Option<&ContextCursorKey>,
    ) -> Self {
        nodes.sort_by(|left, right| {
            (&left.name, left.id.as_str()).cmp(&(&right.name, right.id.as_str()))
        });
        nodes.dedup_by(|left, right| left.id == right.id);
        if let Some(after) = after {
            nodes.retain(|node| {
                (node.name.as_str(), node.id.as_str()) > (after.name.as_str(), after.id.as_str())
            });
        }
        let has_more = nodes.len() > limit;
        nodes.truncate(limit);
        let next = has_more.then(|| {
            let node = nodes
                .last()
                .expect("a positive context limit has a last item");
            ContextCursorKey {
                name: node.name.clone(),
                id: node.id.to_string(),
            }
        });
        Self {
            items: nodes,
            has_more,
            next,
        }
    }
}

impl ContextSection<String> {
    /// Build a process-id page while retaining the process name in the cursor
    /// tie-break key. The returned legacy items remain canonical process IDs.
    pub fn from_process_probe(
        mut processes: Vec<(String, String)>,
        limit: usize,
        after: Option<&ContextCursorKey>,
    ) -> Self {
        processes.sort();
        processes.dedup_by(|left, right| left.1 == right.1);
        if let Some(after) = after {
            processes.retain(|(name, id)| {
                (name.as_str(), id.as_str()) > (after.name.as_str(), after.id.as_str())
            });
        }
        let has_more = processes.len() > limit;
        processes.truncate(limit);
        let next = has_more.then(|| {
            let (name, id) = processes
                .last()
                .expect("a positive context limit has a last item");
            ContextCursorKey {
                name: name.clone(),
                id: id.clone(),
            }
        });
        Self {
            items: processes.into_iter().map(|(_, id)| id).collect(),
            has_more,
            next,
        }
    }
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

/// Stable key for continuing a batched stored-orientation one-hop query.
/// Transports should wrap it in an authenticated, operation-bound cursor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionCursorKey {
    pub source_id: NodeId,
    pub target_name: String,
    pub target_id: NodeId,
    pub edge_kind: String,
    pub traversed_reverse: bool,
    pub stored_edge_token: String,
}

/// Bounds and selection for a stored-orientation transition page.
#[derive(Clone, Debug)]
pub struct TransitionQuery {
    pub direction: Direction,
    /// Empty selects every stored relationship kind.
    pub edge_kinds: Vec<EdgeKind>,
    pub page_limit: usize,
    pub after: Option<TransitionCursorKey>,
}

impl TransitionQuery {
    pub fn validate(&self, source_count: usize) -> Result<()> {
        if source_count > EXECUTION_BATCH_SIZE {
            return Err(GraphStoreError::InvalidInput(format!(
                "stored transition batch {source_count} exceeds {EXECUTION_BATCH_SIZE}"
            )));
        }
        validate_transition_page_limit(self.page_limit)
    }
}

/// A one-hop transition relative to one requested source while retaining the
/// relationship's stored source/destination orientation in `edge`.
#[derive(Clone, Debug)]
pub struct StoredTransition {
    pub source: NodeId,
    pub target: Node,
    pub edge: Edge,
    pub traversed_reverse: bool,
    pub stored_edge_token: String,
}

impl StoredTransition {
    pub fn cursor_key(&self) -> TransitionCursorKey {
        TransitionCursorKey {
            source_id: self.source.clone(),
            target_name: self.target.name.clone(),
            target_id: self.target.id.clone(),
            edge_kind: self.edge.kind.cypher_label().to_string(),
            traversed_reverse: self.traversed_reverse,
            stored_edge_token: self.stored_edge_token.clone(),
        }
    }
}

/// One deterministic adapter page. `backend_limited` is true only when the
/// `limit + 1` probe observed another row; in that case `next_cursor` must be
/// present. A backend that cannot provide a continuation reports
/// `backend_limited=true` with no cursor, making the shared walk inconclusive.
#[derive(Clone, Debug)]
pub struct TransitionBatch {
    pub transitions: Vec<StoredTransition>,
    pub next_cursor: Option<TransitionCursorKey>,
    pub backend_limited: bool,
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

/// One bounded page of kind-aware test coverage (see
/// [`GraphStore::test_coverage_page`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestCoveragePage {
    pub tests: Vec<Node>,
    /// True when the backend's `limit + 1` probe proved more tests exist.
    pub has_more: bool,
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

fn validate_transition_page_limit(limit: usize) -> Result<()> {
    if !(1..=TRANSITION_PAGE_MAX).contains(&limit) {
        return Err(GraphStoreError::InvalidInput(format!(
            "transition page_limit {limit} is outside 1..={TRANSITION_PAGE_MAX}"
        )));
    }
    Ok(())
}

/// Deterministic identity for a reduced stored relationship. Loaders currently
/// merge each `(source, target, kind)` key, so this canonical token is stable
/// across both adapters and across pages without exposing backend internal IDs.
pub fn stored_edge_token(src: &NodeId, dst: &NodeId, kind: EdgeKind) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        src.as_str(),
        dst.as_str(),
        kind.cypher_label()
    )
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

    /// Probe backend connectivity/restore state without graph DDL or a graph
    /// data query. Remote adapters must override this method; the default is
    /// appropriate only for embedded stores whose constructor opens the local
    /// database before returning.
    async fn backend_readiness(&self) -> Result<BackendReadiness> {
        Ok(BackendReadiness::ready())
    }

    // ---- reads (domain queries) ----
    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>>;
    async fn neighbors(&self, id: &NodeId, dir: Direction, kinds: &[EdgeKind])
        -> Result<Vec<Edge>>;
    /// Return one deterministic, keyset-paged batch of stored-orientation
    /// relationships for at most 256 source nodes.
    async fn batched_transitions(
        &self,
        _sources: &[NodeId],
        _query: &TransitionQuery,
    ) -> Result<TransitionBatch> {
        Err(GraphStoreError::Unimplemented("batched_transitions"))
    }
    /// Shared, backend-neutral bounded CALLS impact traversal.
    async fn impact_bounded(&self, id: &NodeId, filter: &ImpactFilter) -> Result<ImpactPage> {
        traversal::impact(self, id, filter).await
    }
    /// Compatibility wrapper retaining the original payload and fixed 200-node
    /// result page. Completeness remains visible through the legacy metadata.
    async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact> {
        Ok(self
            .impact_bounded(id, &ImpactFilter::compatibility(dir, max_depth))
            .await?
            .impact)
    }
    async fn call_chain(&self, from: &NodeId, to: &NodeId, max_depth: u32) -> Result<Vec<Path>>;
    /// Shared multi-root, stored-orientation expansion with explicit bounds.
    async fn subgraph_bounded(
        &self,
        seeds: &[NodeId],
        filter: &SubgraphFilter,
    ) -> Result<SubgraphPage> {
        traversal::subgraph(self, seeds, filter).await
    }
    /// Compatibility wrapper retaining the original `Subgraph` payload while
    /// enforcing deterministic node and edge bounds.
    async fn subgraph(&self, seeds: &[NodeId], radius: u32) -> Result<Subgraph> {
        let page = self
            .subgraph_bounded(seeds, &SubgraphFilter::compatibility(radius))
            .await?;
        Ok(Subgraph {
            nodes: page.nodes.into_iter().map(|item| item.node).collect(),
            edges: page.edges,
        })
    }
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
    /// Return independently paged callers, callees, and process membership.
    ///
    /// The default keeps lightweight test and third-party stores source
    /// compatible by paging their exact legacy context in memory. Production
    /// adapters override it so the bounds are enforced by the backend query.
    async fn context_page(&self, id: &NodeId, filter: &ContextFilter) -> Result<ContextPage> {
        filter.validate()?;
        let context = self.context(id).await?;
        Ok(ContextPage {
            node: context.node,
            callers: ContextSection::from_node_probe(
                context.callers,
                filter.caller_limit,
                filter.caller_after.as_ref(),
            ),
            callees: ContextSection::from_node_probe(
                context.callees,
                filter.callee_limit,
                filter.callee_after.as_ref(),
            ),
            processes: ContextSection::from_process_probe(
                context
                    .processes
                    .into_iter()
                    .map(|id| (id.clone(), id))
                    .collect(),
                filter.process_limit,
                filter.process_after.as_ref(),
            ),
            community: context.community,
        })
    }
    async fn communities(&self) -> Result<Vec<CommunityInfo>>;
    async fn route_map(&self, prefix: Option<&str>, limit: usize) -> Result<Vec<RouteInfo>>;

    // ---- Phase 19: disambiguation + change detection ----

    /// Find all nodes whose simple `name` property matches exactly (case-sensitive).
    /// Returns up to `limit` candidates. Used for ambiguous-symbol detection when
    /// the caller supplies a short name without a kind prefix.
    async fn candidates_by_name(&self, name: &str, limit: usize) -> Result<Vec<Node>>;

    /// Find nodes whose `file` property is in `files` (repo-relative paths),
    /// scoped to callable/structural kinds (Method, Constructor, Function, Class,
    /// Interface, Enum) and ordered deterministically by `(file, id)`. Returns at
    /// most `limit` rows so a single huge generated file cannot force an unbounded
    /// in-memory load; callers that need to detect truncation over-fetch by one.
    /// Used by `detect_changes` to map changed files → symbols.
    async fn nodes_in_files(&self, files: &[String], limit: usize) -> Result<Vec<Node>>;

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

    /// Kind-aware, caller-bounded test coverage with an honest `limit + 1`
    /// over-fetch probe — unlike [`test_coverage`](GraphStore::test_coverage),
    /// whose hard-coded cap silently truncates. Scope depends on the queried
    /// node's kind: Class/Interface additionally include tests targeting
    /// members reached via the type's outgoing HAS_METHOD edges;
    /// Method/Constructor keep the member→owner roll-up; every other kind is
    /// direct-TESTS only. Results are deterministic (`file`, `name`, then `id`
    /// tie-breaker) and deduplicated. A missing node yields an empty complete
    /// page. Adapters must push `LIMIT limit + 1` into the backend query.
    async fn test_coverage_page(&self, id: &NodeId, limit: usize) -> Result<TestCoveragePage>;

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
