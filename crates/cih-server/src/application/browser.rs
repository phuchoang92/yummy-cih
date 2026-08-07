//! Typed query services used by the graph-browser and readiness HTTP adapters.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cih_core::{Node, NodeId};
use cih_graph_store::{
    BackendReadinessState, CommunityEdge, CommunityInfo, Direction, FlowHop, GraphOverview,
    GraphStore, GraphStoreError, GraphSummary, Impact, RouteInfo, Subgraph, SymbolContext,
};
use cih_search::SearchHit;
use serde::Serialize;

use crate::application::search::{
    bounded_subgraph, contain_expansion_page, expansion_seeds, ExpansionBounds, ExpansionLimits,
};
use crate::domain::error::AppError;
use crate::domain::readiness::{ReadinessIssue, ReadinessReport, ReadinessState};
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking};
use crate::ports::graph_readiness::GraphReadiness;
use crate::ports::search_provider::SearchProvider;

/// Hard ceiling on `graph_overview` materialization, enforced here so every
/// caller is bounded before the store builds overview rows in memory — the
/// HTTP adapter's query-param clamp reuses these values, but the application
/// layer must not rely on any one transport doing so.
pub(crate) const OVERVIEW_NODE_CAP: usize = 20_000;
pub(crate) const OVERVIEW_EDGE_CAP: usize = 100_000;

fn overview_ceiling(max_nodes: usize, max_edges: usize) -> (usize, usize) {
    (
        max_nodes.clamp(1, OVERVIEW_NODE_CAP),
        max_edges.clamp(1, OVERVIEW_EDGE_CAP),
    )
}

#[derive(Debug, Serialize)]
pub(crate) struct BrowserSearchResult {
    pub(crate) hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subgraph: Option<Subgraph>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expansion: Option<ExpansionBounds>,
}

pub(crate) struct BrowserFlow {
    pub(crate) entry_node: Option<Node>,
    pub(crate) hops: Vec<FlowHop>,
}

pub(crate) struct BrowserCommunities {
    pub(crate) communities: Vec<CommunityInfo>,
    pub(crate) edges: Vec<CommunityEdge>,
}

#[derive(Clone)]
pub(crate) struct GraphBrowserService {
    store: Arc<dyn GraphStore>,
    search: Arc<dyn SearchProvider>,
    graph_key: Option<String>,
}

impl GraphBrowserService {
    pub(crate) fn new(store: Arc<dyn GraphStore>, search: Arc<dyn SearchProvider>) -> Self {
        Self {
            store,
            search,
            graph_key: None,
        }
    }

    pub(crate) fn with_graph_key(mut self, graph_key: String) -> Self {
        self.graph_key = Some(graph_key);
        self
    }

    pub(crate) async fn summary(&self) -> Result<GraphSummary, AppError> {
        if let Some(graph_key) = self.graph_key.clone() {
            match run_blocking(blocking_timeout(), "graph summary metadata", move || {
                cih_core::Registry::load_cached()
                    .entries
                    .iter()
                    .find(|entry| entry.graph_key == graph_key)
                    .and_then(crate::application::graph_report::current)
                    .map(crate::application::graph_report::summary)
            })
            .await
            {
                Ok(Some(summary)) => return Ok(summary),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    error = %error,
                    "graph summary metadata unavailable; using live graph fallback"
                ),
            }
        }
        self.store.graph_summary().await.map_err(graph_error)
    }

    pub(crate) async fn overview(
        &self,
        max_nodes: usize,
        max_edges: usize,
        kinds: Option<&[String]>,
    ) -> Result<GraphOverview, AppError> {
        let (max_nodes, max_edges) = overview_ceiling(max_nodes, max_edges);
        self.store
            .graph_overview(max_nodes, max_edges, kinds)
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        limit: usize,
        expand: bool,
        expansion_limits: ExpansionLimits,
    ) -> Result<BrowserSearchResult, AppError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(AppError::InvalidInput {
                field: "q",
                message: "query parameter is required".into(),
            });
        }
        let hits = self
            .search
            .query_hits(query, limit)
            .await
            .map_err(|error| {
                let retryable = error.retryable();
                AppError::Unavailable {
                    dependency: "search index",
                    message: error.to_string(),
                    retryable,
                }
            })?;
        let subgraph = if expand && !hits.is_empty() {
            let seeds = expansion_seeds(&hits);
            let page = bounded_subgraph(self.store.as_ref(), &seeds, expansion_limits).await?;
            let contained = contain_expansion_page(&hits, seeds, page, expansion_limits)?;
            return Ok(BrowserSearchResult {
                hits: contained.hits,
                subgraph: contained.subgraph,
                expansion: contained.expansion,
            });
        } else {
            None
        };
        Ok(BrowserSearchResult {
            hits,
            subgraph,
            expansion: None,
        })
    }

    pub(crate) async fn context(&self, id: &NodeId) -> Result<SymbolContext, AppError> {
        self.store.context(id).await.map_err(graph_error)
    }

    pub(crate) async fn impact(
        &self,
        id: &NodeId,
        direction: Direction,
        depth: u32,
    ) -> Result<Impact, AppError> {
        self.store
            .impact(id, direction, depth)
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn flow(
        &self,
        entry_id: &NodeId,
        depth: u32,
    ) -> Result<BrowserFlow, AppError> {
        let hops = self
            .store
            .flow_downstream(entry_id, &cih_graph_store::FlowFilter::depth(depth))
            .await
            .map_err(graph_error)?
            .hops;
        let entry_node = self.store.get_node(entry_id).await.map_err(graph_error)?;
        Ok(BrowserFlow { entry_node, hops })
    }

    pub(crate) async fn communities(&self) -> Result<BrowserCommunities, AppError> {
        let communities = self.store.communities().await.map_err(graph_error)?;
        let edges = self.store.community_graph().await.map_err(graph_error)?;
        Ok(BrowserCommunities { communities, edges })
    }

    pub(crate) async fn routes(
        &self,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RouteInfo>, AppError> {
        self.store
            .route_map(prefix, limit)
            .await
            .map_err(graph_error)
    }
}

#[derive(Clone)]
pub(crate) struct ReadinessService {
    probe: Arc<dyn GraphReadiness>,
    artifacts_dir: Option<PathBuf>,
    cache: Arc<tokio::sync::Mutex<Option<(Instant, ReadinessReport)>>>,
}

const READINESS_CACHE_TTL: Duration = Duration::from_secs(1);
const READINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
impl ReadinessService {
    pub(crate) fn new(probe: Arc<dyn GraphReadiness>, artifacts_dir: Option<PathBuf>) -> Self {
        Self {
            probe,
            artifacts_dir,
            cache: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub(crate) async fn check(&self) -> ReadinessReport {
        // Hold the small readiness lock through the probe. Concurrent callers
        // then share one backend check instead of producing a restore-time
        // query stampede.
        let mut cache = self.cache.lock().await;
        if let Some((checked_at, report)) = cache.as_ref() {
            if checked_at.elapsed() < READINESS_CACHE_TTL {
                return report.clone();
            }
        }

        let mut state = ReadinessState::Ready;
        let mut issues = Vec::new();
        match tokio::time::timeout(READINESS_PROBE_TIMEOUT, self.probe.backend_readiness()).await {
            Ok(Ok(backend)) if backend.state == BackendReadinessState::Ready => {}
            Ok(Ok(backend)) => {
                state = ReadinessState::BackendLoading;
                issues.push(ReadinessIssue {
                    code: "BACKEND_LOADING",
                    message: backend
                        .detail
                        .unwrap_or_else(|| "graph backend is restoring persisted data".into()),
                    retryable: true,
                    retry_after_ms: backend.retry_after_ms,
                });
            }
            Ok(Err(error)) => {
                if matches!(error, GraphStoreError::Loading { .. }) {
                    state = ReadinessState::BackendLoading;
                } else {
                    state = ReadinessState::Degraded;
                }
                issues.push(readiness_issue(error));
            }
            Err(_) => {
                state = ReadinessState::Degraded;
                issues.push(ReadinessIssue {
                    code: "BACKEND_PROBE_TIMEOUT",
                    message: format!(
                        "graph readiness probe exceeded {}ms",
                        READINESS_PROBE_TIMEOUT.as_millis()
                    ),
                    retryable: true,
                    retry_after_ms: Some(READINESS_CACHE_TTL.as_millis() as u64),
                });
            }
        }
        if self
            .artifacts_dir
            .as_ref()
            .is_some_and(|directory| !directory.exists())
        {
            if state == ReadinessState::Ready {
                state = ReadinessState::Degraded;
            }
            issues.push(ReadinessIssue {
                code: "ARTIFACTS_MISSING",
                message: "artifacts directory not found".to_string(),
                retryable: false,
                retry_after_ms: None,
            });
        }
        let report = ReadinessReport::new(state, issues);
        *cache = Some((Instant::now(), report.clone()));
        report
    }

    /// Reject graph reads from the cached backend snapshot before they reach
    /// an adapter. Non-graph capabilities remain available while Redis is
    /// restoring a large persisted dataset.
    pub(crate) async fn admit_graph_read(&self) -> Result<(), AppError> {
        let report = self.check().await;
        let Some(issue) = report.backend_issue() else {
            return Ok(());
        };
        Err(AppError::GraphUnavailable {
            code: issue.code,
            message: issue.message.clone(),
            retryable: issue.retryable,
            retry_after_ms: issue.retry_after_ms,
        })
    }
}

fn readiness_issue(error: GraphStoreError) -> ReadinessIssue {
    let retry = error.retry_metadata();
    let retryable = retry
        .map(|metadata| metadata.retryable)
        .unwrap_or(matches!(&error, GraphStoreError::Backend(_)));
    let retry_after_ms = retry.and_then(|metadata| metadata.retry_after_ms);
    let code = match &error {
        GraphStoreError::Loading { .. } => "BACKEND_LOADING",
        GraphStoreError::Overloaded { .. } => "BACKEND_OVERLOADED",
        GraphStoreError::ExecutionTimeout { .. } => "BACKEND_TIMEOUT",
        GraphStoreError::Unavailable { .. } | GraphStoreError::Backend(_) => "BACKEND_UNAVAILABLE",
        GraphStoreError::Index { .. } => "INDEX_UNAVAILABLE",
        GraphStoreError::NotFound(_) => "GRAPH_NOT_FOUND",
        GraphStoreError::Unimplemented(_) => "BACKEND_UNIMPLEMENTED",
        GraphStoreError::InvalidInput(_) => "READINESS_PROBE_INVALID",
        GraphStoreError::Other(_) => "BACKEND_ERROR",
    };
    ReadinessIssue {
        code,
        message: error.to_string(),
        retryable,
        retry_after_ms,
    }
}

fn graph_error(error: GraphStoreError) -> AppError {
    AppError::from_graph_store(error, "node")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cih_graph_store::BackendReadiness;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn overview_ceiling_bounds_every_caller() {
        assert_eq!(overview_ceiling(0, 0), (1, 1));
        assert_eq!(overview_ceiling(5_000, 25_000), (5_000, 25_000));
        assert_eq!(
            overview_ceiling(usize::MAX, usize::MAX),
            (OVERVIEW_NODE_CAP, OVERVIEW_EDGE_CAP)
        );
    }

    #[test]
    fn graph_errors_map_to_transport_independent_variants() {
        assert!(matches!(
            graph_error(GraphStoreError::NotFound("Method:x".into())),
            AppError::NotFound {
                entity: "node",
                key
            } if key == "Method:x"
        ));
        assert!(matches!(
            graph_error(GraphStoreError::Backend("down".into())),
            AppError::GraphUnavailable {
                code: "BACKEND_UNAVAILABLE",
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn loading_readiness_error_keeps_typed_retry_guidance() {
        let issue = readiness_issue(GraphStoreError::Loading {
            message: "restore in progress".into(),
            retry: cih_graph_store::RetryMetadata::retryable(Some(750)),
        });

        assert_eq!(issue.code, "BACKEND_LOADING");
        assert_eq!(issue.retry_after_ms, Some(750));
    }

    struct CountingReadiness {
        calls: AtomicUsize,
        response: BackendReadiness,
    }

    #[async_trait]
    impl GraphReadiness for CountingReadiness {
        async fn backend_readiness(&self) -> cih_graph_store::Result<BackendReadiness> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn readiness_single_flights_and_caches_restore_state() {
        let probe = Arc::new(CountingReadiness {
            calls: AtomicUsize::new(0),
            response: BackendReadiness::loading("restore in progress", 1_000),
        });
        let service = ReadinessService::new(probe.clone(), None);

        let checks = (0..32).map(|_| {
            let service = service.clone();
            tokio::spawn(async move { service.check().await })
        });
        for check in checks {
            let report = check.await.expect("readiness task completes");
            assert_eq!(report.state, ReadinessState::BackendLoading);
            assert_eq!(report.retry_after_ms, Some(1_000));
            assert_eq!(report.issues.len(), 1);
            assert_eq!(report.issues[0].code, "BACKEND_LOADING");
        }

        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        let cached = service.check().await;
        assert_eq!(cached.state, ReadinessState::BackendLoading);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

        let error = service
            .admit_graph_read()
            .await
            .expect_err("loading backend rejects graph admission");
        assert!(matches!(
            error,
            AppError::GraphUnavailable {
                code: "BACKEND_LOADING",
                retryable: true,
                retry_after_ms: Some(1_000),
                ..
            }
        ));
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            1,
            "admission consumes the same cached snapshot"
        );
    }
}
