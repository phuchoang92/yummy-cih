//! Typed query services used by the graph-browser and readiness HTTP adapters.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cih_core::{Node, NodeId};
use cih_graph_store::{
    BackendReadinessState, CommunityEdge, CommunityInfo, Direction, FlowHop, GraphOverview,
    GraphStoreError, GraphSummary, Impact, RouteInfo, Subgraph, SymbolContext,
};
use cih_search::SearchHit;
use serde::Serialize;

use crate::application::search::{
    bounded_subgraph, contain_expansion_page, expansion_seeds, ExpansionBounds, ExpansionLimits,
};
use crate::domain::error::AppError;
use crate::domain::readiness::{ReadinessIssue, ReadinessReport, ReadinessState};
use crate::domain::repository::RepoSelector;
use crate::ports::graph_readiness::GraphReadiness;
use crate::ports::repo_context_provider::{RepoContext, RepoContextProvider};

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
    repos: Arc<dyn RepoContextProvider>,
}

impl GraphBrowserService {
    pub(crate) fn new(repos: Arc<dyn RepoContextProvider>) -> Self {
        Self { repos }
    }

    async fn resolve_context(&self) -> Result<Arc<RepoContext>, AppError> {
        self.repos.resolve(RepoSelector::Default).await
    }

    pub(crate) async fn summary(&self) -> Result<GraphSummary, AppError> {
        let repo = self.repos.resolve_repo(RepoSelector::Default)?;
        if let Some(report) = crate::application::graph_report::current(&repo.registry_entry) {
            return Ok(crate::application::graph_report::summary(report));
        }
        self.resolve_context()
            .await?
            .store
            .graph_summary()
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn overview(
        &self,
        max_nodes: usize,
        max_edges: usize,
        kinds: Option<&[String]>,
    ) -> Result<GraphOverview, AppError> {
        let (max_nodes, max_edges) = overview_ceiling(max_nodes, max_edges);
        let context = self.resolve_context().await?;
        let overview = context
            .store
            .graph_overview(max_nodes, max_edges, kinds)
            .await
            .map_err(graph_error)?;
        if kinds.is_none() && overview.nodes.is_empty() {
            if let Some(report) =
                crate::application::graph_report::current(&context.repo.registry_entry)
            {
                if report.total_nodes > 0 {
                    let live = context.store.graph_summary().await.map_err(graph_error)?;
                    if live.total_nodes == 0 {
                        tracing::error!(
                            logical_graph_key = context.repo.graph_key(),
                            published_epoch = ?context.repo.registry_entry.published_epoch,
                            expected_nodes = report.total_nodes,
                            "published graph metadata does not match the resolved browser store"
                        );
                        return Err(AppError::GraphUnavailable {
                            code: "GRAPH_PUBLICATION_MISMATCH",
                            message: "published graph metadata exists, but the resolved graph store is empty"
                                .into(),
                            retryable: false,
                            retry_after_ms: None,
                        });
                    }
                }
            }
        }
        Ok(overview)
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
        // Resolve once so search hits and any graph expansion are pinned to the
        // same repository publication for the lifetime of this request.
        let context = self.resolve_context().await?;
        let hits = context
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
            let page = bounded_subgraph(context.store.as_ref(), &seeds, expansion_limits).await?;
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
        self.resolve_context()
            .await?
            .store
            .context(id)
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn impact(
        &self,
        id: &NodeId,
        direction: Direction,
        depth: u32,
    ) -> Result<Impact, AppError> {
        self.resolve_context()
            .await?
            .store
            .impact(id, direction, depth)
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn flow(
        &self,
        entry_id: &NodeId,
        depth: u32,
    ) -> Result<BrowserFlow, AppError> {
        let context = self.resolve_context().await?;
        let hops = context
            .store
            .flow_downstream(entry_id, &cih_graph_store::FlowFilter::depth(depth))
            .await
            .map_err(graph_error)?
            .hops;
        let entry_node = context
            .store
            .get_node(entry_id)
            .await
            .map_err(graph_error)?;
        Ok(BrowserFlow { entry_node, hops })
    }

    pub(crate) async fn communities(&self) -> Result<BrowserCommunities, AppError> {
        let context = self.resolve_context().await?;
        let communities = context.store.communities().await.map_err(graph_error)?;
        let edges = context.store.community_graph().await.map_err(graph_error)?;
        Ok(BrowserCommunities { communities, edges })
    }

    pub(crate) async fn routes(
        &self,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RouteInfo>, AppError> {
        self.resolve_context()
            .await?
            .store
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
    use cih_core::{GraphArtifacts, NodeKind, Range, VersionId};
    use cih_graph_store::{BackendReadiness, GraphStore};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::RwLock;

    use crate::domain::repository::{RepoCatalogSnapshot, ResolvedRepo};
    use crate::infrastructure::search_provider::SearchState;

    struct SwitchingRepoContexts {
        current: RwLock<Arc<RepoContext>>,
        resolve_calls: AtomicUsize,
    }

    impl SwitchingRepoContexts {
        fn new(context: Arc<RepoContext>) -> Self {
            Self {
                current: RwLock::new(context),
                resolve_calls: AtomicUsize::new(0),
            }
        }

        fn replace(&self, context: Arc<RepoContext>) {
            *self
                .current
                .write()
                .unwrap_or_else(|error| error.into_inner()) = context;
        }

        fn current(&self) -> Arc<RepoContext> {
            self.current
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl RepoContextProvider for SwitchingRepoContexts {
        fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
            panic!("graph browser operations do not request a catalog snapshot")
        }

        fn resolve_repo(&self, _selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
            Ok(self.current().repo.clone())
        }

        async fn resolve(&self, _selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.current())
        }
    }

    fn registry_entry(path: &std::path::Path, epoch: &str) -> cih_core::RegistryEntry {
        cih_core::RegistryEntry {
            repository_id: None,
            name: "browser-publication-test".into(),
            path: path.to_string_lossy().into_owned(),
            graph_key: "logical-key".into(),
            artifacts_dir: String::new(),
            latest_artifact_version: None,
            published_artifact_version: None,
            published_graph_content_version: None,
            published_epoch: Some(epoch.into()),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        }
    }

    fn reported_registry_entry(path: &std::path::Path) -> cih_core::RegistryEntry {
        let mut entry = registry_entry(path, "published-epoch");
        entry.published_artifact_version = Some("base-v1".into());
        entry.published_graph_content_version = Some("content-v1".into());
        entry.stats.published_graph_report = Some(cih_core::RegistryGraphReport {
            schema_version: 1,
            graph_content_version: "content-v1".into(),
            total_nodes: 1,
            total_edges: 0,
            kinds: vec![cih_core::RegistryKindCount {
                kind: "Method".into(),
                count: 1,
            }],
            symbol_hubs: Vec::new(),
        });
        entry
    }

    async fn graph_context(
        root: &std::path::Path,
        key: &str,
        epoch: &str,
        node_names: &[&str],
    ) -> Arc<RepoContext> {
        let store = cih_ladybug::LadybugStore::connect(&root.to_string_lossy(), key)
            .expect("connect embedded browser test graph");
        let nodes = node_names
            .iter()
            .map(|name| Node {
                id: NodeId::new(format!("Method:test.Service#{name}/0")),
                kind: NodeKind::Method,
                name: (*name).to_string(),
                qualified_name: Some(format!("test.Service::{name}")),
                file: "src/service.rs".into(),
                range: Range::default(),
                props: None,
            })
            .collect::<Vec<_>>();
        let artifacts = GraphArtifacts::write(
            &root.join(format!("artifacts-{epoch}")),
            VersionId::new(epoch),
            &nodes,
            &[],
        )
        .expect("write browser graph artifacts");
        store
            .bulk_load(&artifacts)
            .await
            .expect("load browser graph artifacts");
        Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(registry_entry(root, epoch)),
            store: Arc::new(store),
            search: Arc::new(SearchState::new(None, None)),
        })
    }

    #[test]
    fn overview_ceiling_bounds_every_caller() {
        assert_eq!(overview_ceiling(0, 0), (1, 1));
        assert_eq!(overview_ceiling(5_000, 25_000), (5_000, 25_000));
        assert_eq!(
            overview_ceiling(usize::MAX, usize::MAX),
            (OVERVIEW_NODE_CAP, OVERVIEW_EDGE_CAP)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graph_browser_resolves_fresh_publication_context_per_request() {
        let temp = tempfile::tempdir().expect("browser publication test root");
        let epoch_a = graph_context(temp.path(), "physical-a", "epoch-a", &["alpha"]).await;
        let epoch_b = graph_context(temp.path(), "physical-b", "epoch-b", &["beta", "gamma"]).await;
        let provider = Arc::new(SwitchingRepoContexts::new(epoch_a));
        let service = GraphBrowserService::new(provider.clone());
        let kinds = ["Method".to_string()];

        let first = service.overview(100, 100, Some(&kinds)).await.unwrap();
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(provider.resolve_calls.load(Ordering::SeqCst), 1);

        let first_summary = service.summary().await.unwrap();
        assert_eq!(first_summary.total_nodes, 1);
        assert_eq!(provider.resolve_calls.load(Ordering::SeqCst), 2);

        provider.replace(epoch_b);

        let second = service.overview(100, 100, Some(&kinds)).await.unwrap();
        assert_eq!(second.nodes.len(), 2);
        assert!(second.nodes.iter().any(|node| node.node.name == "beta"));
        assert!(second.nodes.iter().any(|node| node.node.name == "gamma"));
        assert_eq!(provider.resolve_calls.load(Ordering::SeqCst), 3);

        let second_summary = service.summary().await.unwrap();
        assert_eq!(second_summary.total_nodes, 2);
        assert_eq!(provider.resolve_calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graph_browser_reports_publication_mismatch_instead_of_empty_index_guidance() {
        let temp = tempfile::tempdir().expect("browser mismatch test root");
        let store = cih_ladybug::LadybugStore::connect(
            &temp.path().join("graphs").to_string_lossy(),
            "empty-logical-key",
        )
        .expect("connect empty logical graph");
        let context = Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(reported_registry_entry(temp.path())),
            store: Arc::new(store),
            search: Arc::new(SearchState::new(None, None)),
        });
        let service = GraphBrowserService::new(Arc::new(SwitchingRepoContexts::new(context)));

        let error = service
            .overview(100, 100, None)
            .await
            .expect_err("published metadata plus an empty store is inconsistent");

        assert!(matches!(
            error,
            AppError::GraphUnavailable {
                code: "GRAPH_PUBLICATION_MISMATCH",
                retryable: false,
                ..
            }
        ));
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
