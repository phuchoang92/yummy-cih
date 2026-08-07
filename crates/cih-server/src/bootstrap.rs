//! Process bootstrap: configuration, dependency assembly, transports, and serving.
//! (MCP endpoint, graph browser UI, wiki search, health/ready), and serve until
//! shutdown. Protocol behavior lives under `transport`; this is composition.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::{middleware, routing::get};
#[cfg(feature = "semantic")]
use cih_embed::{EmbedModelKind, EmbedStore};
use cih_graph_store::GraphStore;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;
use tower_http::{
    catch_panic::CatchPanicLayer, compression::CompressionLayer, limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer, trace::TraceLayer,
};

/// Max request body accepted on any route. MCP JSON-RPC payloads and tool
/// arguments are small; this caps memory a large authed POST can force us to
/// buffer.
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Explicit server inputs used by the unified executable. Environment-driven
/// operational limits remain supported, while repository identity and the
/// embedded graph location are fixed by the product entry point.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    pub bind: String,
    pub backend: String,
    pub store_url: String,
    pub graph_key: String,
    pub artifacts_dir: Option<std::path::PathBuf>,
    pub index_program: crate::IndexProgram,
}

use crate::application::admin::resolve_patterns::ResolvePatternService;
use crate::application::admin::RepositoryAdminService;
use crate::application::app_services::{
    AdminUseCases, AppServices, CrossRepoUseCases, DocsUseCases, FileUseCases, GraphUseCases,
    RepoContextService, SearchUseCases, TestingUseCases,
};
use crate::application::architecture_overview::ArchitectureOverviewService;
use crate::application::browser::GraphBrowserService;
use crate::application::browser::ReadinessService;
use crate::application::change_detection::ChangeDetectionService;
use crate::application::contracts::ContractService;
use crate::application::files::{FileService, GrepRuntime, GrepRuntimeConfig, ReadFileLimits};
use crate::application::graph::GraphQueryService;
use crate::application::indexing::IndexingService;
use crate::application::search::SearchService;
use crate::application::taint::TaintService;
use crate::application::testing::TestingService;
use crate::application::wiki_search::{WikiPageService, WikiSearchService};
use crate::config::{store_runtime_options, CacheBudgets, Config, RetrievalConfig};
use crate::infrastructure::artifact_cross_repo_graph::ArtifactCrossRepoGraphProvider;
use crate::infrastructure::artifact_repository::ArtifactCache;
use crate::infrastructure::git_changed_files::GitChangedFilesSource;
use crate::infrastructure::graph_readiness::GraphStoreReadiness;
use crate::infrastructure::graph_store_provider::build_store;
use crate::infrastructure::local_job_scheduler::{IndexScheduler, RegistryIndexTargetResolver};
use crate::infrastructure::repo_context_provider::DefaultRepoContextProvider;
use crate::infrastructure::retrieval_metrics::RuntimeRetrievalMetrics;
use crate::infrastructure::search_provider::{SearchCache, SearchState, SemanticStore};
use crate::infrastructure::wiki_repository::{
    WikiBundlePageRepository, WikiBundleSearchRepository, WikiOverviewRepository, WikiSearchState,
};
use crate::ports::artifact_repository::ArtifactRepository;
use crate::ports::repo_context_provider::RepoContextProvider;
use crate::transport::http::{browser, health, wiki as wiki_http};
use crate::transport::mcp::{CihServer, ResponseGuardConfig};

#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_services(
    store: Arc<dyn GraphStore>,
    artifacts_dir: Option<std::path::PathBuf>,
    embed_store: Option<Arc<SemanticStore>>,
    graph_key: String,
    group: Option<String>,
    backend: String,
    falkor_url: String,
    store_limits: (usize, Duration),
    store_runtime: cih_store_factory::StoreRuntimeOptions,
    search_cache: SearchCache,
    read_file_limits: ReadFileLimits,
    grep_runtime: Arc<GrepRuntime>,
    wiki_state: WikiSearchState,
    index_program: crate::IndexProgram,
) -> Arc<AppServices> {
    let search = SearchState::with_cache(
        artifacts_dir.clone(),
        embed_store.clone(),
        search_cache.clone(),
    );
    let browser_service = GraphBrowserService::new(store.clone(), Arc::new(search.clone()))
        .with_graph_key(graph_key.clone());
    let repo_contexts: Arc<dyn RepoContextProvider> =
        Arc::new(DefaultRepoContextProvider::production(
            graph_key.clone(),
            store,
            search,
            artifacts_dir,
            backend.clone(),
            falkor_url.clone(),
            store_limits,
            store_runtime,
            embed_store,
            search_cache.clone(),
        ));
    let jobs = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let artifacts: Arc<dyn ArtifactRepository> = Arc::new(ArtifactCache::new());
    let index_scheduler = Arc::new(IndexScheduler::new(
        jobs,
        artifacts.clone(),
        backend,
        falkor_url,
        index_program,
    ));
    let indexing_service = IndexingService::new(
        Arc::new(RegistryIndexTargetResolver),
        index_scheduler.clone(),
    );
    let operational_metrics = crate::application::admin::OperationalMetricsService::new(
        index_scheduler,
        Arc::new(RuntimeRetrievalMetrics::new(
            search_cache,
            wiki_state.clone(),
            grep_runtime.clone(),
        )),
    );
    let contract_service = ContractService::new(
        repo_contexts.clone(),
        Arc::new(ArtifactCrossRepoGraphProvider::new(artifacts.clone())),
        artifacts.clone(),
    );
    let architecture_overview = ArchitectureOverviewService::new(
        repo_contexts.clone(),
        Arc::new(WikiOverviewRepository::new()),
    );
    let wiki_search = WikiSearchService::new(
        repo_contexts.clone(),
        Arc::new(WikiBundleSearchRepository::new(wiki_state.clone())),
    );
    let wiki_page = WikiPageService::new(
        repo_contexts.clone(),
        Arc::new(WikiBundlePageRepository::new(wiki_state.clone())),
    );

    let repos = RepoContextService::new(repo_contexts);
    // Named service values shared between their use-case owners and the
    // composite DocPackService (every service is a cheap Clone over Arcs).
    let graph_queries = GraphQueryService::new(
        repos.clone(),
        ChangeDetectionService::new(Arc::new(GitChangedFilesSource)),
    );
    let testing_service = TestingService::new(repos.clone(), TaintService::new(artifacts));
    let file_service = FileService::new(repos.clone(), read_file_limits, grep_runtime);
    let doc_pack = crate::application::doc_pack::DocPackService::new(
        repos.clone(),
        graph_queries.clone(),
        testing_service.clone(),
        file_service.clone(),
        contract_service.clone(),
    );
    Arc::new(AppServices {
        repos: repos.clone(),
        graph: GraphUseCases {
            queries: graph_queries,
            architecture_overview,
            browser: browser_service,
        },
        search: SearchUseCases {
            queries: SearchService::new(repos.clone()),
        },
        cross_repo: CrossRepoUseCases {
            contracts: contract_service,
        },
        testing: TestingUseCases {
            analysis: testing_service,
        },
        docs: DocsUseCases {
            wiki_search,
            wiki_page,
            doc_pack,
        },
        files: FileUseCases {
            access: file_service,
        },
        admin: AdminUseCases {
            repositories: RepositoryAdminService::new(repos.clone(), graph_key, group),
            patterns: ResolvePatternService::new(repos.clone(), indexing_service.clone()),
            indexing: indexing_service,
            operations: operational_metrics,
        },
    })
}

#[cfg(feature = "semantic")]
async fn initialize_optional_semantic<T, F, Fut>(
    pg_url: Option<&str>,
    inference: cih_embed::EmbedInferenceConfig,
    initialize: F,
) -> Result<Option<T>>
where
    F: FnOnce(String, cih_embed::EmbedInferenceConfig) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    match pg_url {
        Some(pg_url) => initialize(pg_url.to_string(), inference).await.map(Some),
        None => Ok(None),
    }
}

/// Start the CIH MCP server: read config from env, build the graph store,
/// assemble the axum app (MCP endpoint, graph browser UI, health/ready), and
/// serve until shutdown.
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cih_server=debug".into()),
        )
        .init();

    let cfg = Config::try_from_env()?;
    run_config(cfg, crate::IndexProgram::legacy()).await
}

/// Start the server from explicit product configuration. Tracing and runtime
/// initialization belong to the caller so a unified executable initializes
/// each process-wide facility exactly once.
pub async fn run_with_config(config: ServeConfig) -> Result<()> {
    let mut cfg = Config::try_from_env()?;
    cfg.bind = config.bind;
    cfg.backend = config.backend;
    cfg.falkor_url = config.store_url;
    cfg.graph_key = config.graph_key;
    cfg.artifacts_dir = config.artifacts_dir;
    run_config(cfg, config.index_program).await
}

async fn run_config(cfg: Config, index_program: crate::IndexProgram) -> Result<()> {
    let cache_budgets = CacheBudgets::from_env()?;
    let retrieval = RetrievalConfig::from_env()?;
    retrieval.validate_operation_deadline(cfg.graph_operation_timeout_ms)?;
    let grep_runtime = Arc::new(
        GrepRuntime::new(GrepRuntimeConfig {
            max_concurrent_requests: retrieval.grep_max_concurrent_requests,
            threads: retrieval.grep_threads,
            queue_timeout: Duration::from_secs(retrieval.grep_queue_timeout_secs),
            deadline: Duration::from_secs(retrieval.grep_deadline_secs),
        })
        .map_err(anyhow::Error::msg)?,
    );
    crate::infrastructure::wiki_repository::validate_live_wiki_config()?;
    tracing::info!(?cfg, "starting CIH MCP server");
    tracing::info!(
        artifact_cache_bytes = cache_budgets.artifact_bytes,
        wiki_cache_bytes = cache_budgets.wiki_bytes,
        search_cache_bytes = cache_budgets.search_bytes,
        resource_index_cache_bytes = cache_budgets.resource_index_bytes,
        total_cache_bytes = cache_budgets.total_bytes,
        "validated process cache budgets"
    );
    tracing::info!(?retrieval, "validated retrieval limits");

    cfg.check_auth_posture()?;
    if cfg.api_token.is_none() {
        tracing::warn!("CIH_API_TOKEN is not set — server is open to unauthenticated requests");
    }
    let cursor_key_configured = crate::application::cursor::signing_key_is_configured();
    cfg.check_cursor_key_posture(cursor_key_configured)?;
    if !cursor_key_configured && !cfg.is_loopback_bind() {
        tracing::warn!(
            setting = "CIH_CURSOR_SIGNING_KEY",
            bind = %cfg.bind,
            "network-exposed bind without a shared cursor key: pagination cursors will not \
             validate across restarts or replicas behind a load balancer; set the same \
             64-hex secret on every instance"
        );
    }
    let store = build_store(&cfg).await?;
    let search_cache = SearchCache::from_config(&retrieval, cache_budgets.search_bytes);
    if let Some(artifacts_dir) = cfg.artifacts_dir.as_deref() {
        search_cache.preflight_artifacts(artifacts_dir);
    }
    #[cfg(feature = "semantic")]
    let embed_store = initialize_optional_semantic(
        cfg.pg_url.as_deref(),
        retrieval.embed_inference,
        |pg_url, inference| async move {
            let store = EmbedStore::connect_with_inference_config(
                &pg_url,
                EmbedModelKind::MiniLm,
                inference,
            )
            .await?;
            store.ensure_schema().await?;
            Ok(Arc::new(store))
        },
    )
    .await?;
    #[cfg(not(feature = "semantic"))]
    let embed_store: Option<Arc<SemanticStore>> = {
        if cfg.pg_url.is_some() {
            tracing::warn!("CIH_PG_URL ignored: this CIH build includes BM25 search only");
        }
        None
    };
    let graph_key = cfg.graph_key.clone();
    // One shared state: the axum /wiki/search route and the MCP wiki tools use
    // the same mtime-invalidated index cache.
    let wiki_state = WikiSearchState::new();
    let services = assemble_services(
        store.clone(),
        cfg.artifacts_dir.clone(),
        embed_store,
        graph_key,
        cfg.group.clone(),
        cfg.backend.clone(),
        cfg.falkor_url.clone(),
        (
            cfg.max_concurrent_queries,
            Duration::from_millis(cfg.query_queue_timeout_ms),
        ),
        store_runtime_options(&cfg),
        search_cache,
        ReadFileLimits {
            max_bytes: cfg.read_file_max_bytes,
            max_lines: cfg.read_file_max_lines,
        },
        grep_runtime,
        wiki_state.clone(),
        index_program,
    );
    let observability: Arc<dyn crate::ports::observability::ObservabilityPort> =
        Arc::new(crate::infrastructure::tracing_observability::TracingObservability);
    let ready_state = ReadinessService::new(
        Arc::new(GraphStoreReadiness::new(store)),
        cfg.artifacts_dir.clone(),
    );
    let cih = CihServer::with_observability(services.clone(), observability.clone())
        .with_graph_readiness(ready_state.clone())
        .with_operation_timeout(Duration::from_millis(cfg.graph_operation_timeout_ms))
        .with_response_guard(ResponseGuardConfig::new(
            cfg.mcp_response_target_bytes,
            cfg.mcp_response_max_bytes,
            cfg.mcp_response_guard_mode,
        ));
    let browser_state =
        browser::BrowserState::new(services.graph.browser.clone(), cfg.artifacts_dir.clone());
    let wiki_search_service = services.docs.wiki_search.clone();

    let service = StreamableHttpService::new(
        move || Ok(cih.clone()),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );

    let browser_routes = browser::router(browser_state)
        .layer(middleware::from_fn_with_state(
            ready_state.clone(),
            health::graph_readiness_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            observability.clone(),
            health::observability_middleware,
        ));
    // MCP tool calls are observed at the RMCP dispatch boundary. Applying the
    // HTTP middleware to `/mcp` as well would double-count them.
    let operations_routes = axum::Router::new()
        .route(
            "/operations/metrics",
            get(health::operational_metrics_handler).with_state(services.admin.operations.clone()),
        )
        .layer(middleware::from_fn_with_state(
            observability.clone(),
            health::observability_middleware,
        ));
    let protected = axum::Router::new()
        .nest_service("/mcp", service)
        .merge(browser_routes)
        .merge(operations_routes)
        .layer(middleware::from_fn_with_state(
            cfg.api_token.clone(),
            health::auth_middleware,
        ));

    // Wiki search is fetched by browsers (docs-viewer), so it needs CORS.
    // The CorsLayer must wrap the auth middleware (layers run outermost-last):
    // OPTIONS preflights carry no Authorization header and would otherwise 401
    // whenever CIH_API_TOKEN is set.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([axum::http::Method::GET])
        .allow_headers([axum::http::header::AUTHORIZATION]);
    let wiki_routes = wiki_http::router(wiki_search_service)
        .layer(middleware::from_fn_with_state(
            observability.clone(),
            health::observability_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            cfg.api_token.clone(),
            health::auth_middleware,
        ))
        .layer(cors);

    let public = axum::Router::new()
        .route("/health", get(health::health_handler))
        .route("/ready", get(health::ready_handler).with_state(ready_state))
        .layer(middleware::from_fn_with_state(
            observability,
            health::observability_middleware,
        ));

    let app = public
        .merge(protected)
        .merge(wiki_routes)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(120),
        ))
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        // Outermost: turn a panic in any inner layer/handler into a 500 instead of
        // dropping the client connection.
        .layer(CatchPanicLayer::new());

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("MCP (Streamable HTTP) listening on http://{}/mcp", cfg.bind);
    tracing::info!("CIH graph browser listening on http://{}/graph", cfg.bind);
    tracing::info!("wiki search listening on http://{}/wiki/search", cfg.bind);

    axum::serve(listener, app)
        .with_graceful_shutdown(health::shutdown_signal())
        .await?;
    tracing::info!("server shut down cleanly");
    Ok(())
}

#[cfg(all(test, feature = "semantic"))]
mod tests {
    use super::initialize_optional_semantic;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn semantic_bootstrap_passes_the_validated_inference_config_unchanged() {
        let expected = cih_embed::EmbedInferenceConfig::new(
            2,
            Duration::from_millis(125),
            Duration::from_millis(750),
        )
        .unwrap();
        let initialized = initialize_optional_semantic(
            Some("postgres://semantic.test/cih"),
            expected,
            |pg_url, inference| async move { Ok::<_, anyhow::Error>((pg_url, inference)) },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(initialized.0, "postgres://semantic.test/cih");
        assert_eq!(initialized.1, expected);
    }

    #[tokio::test]
    async fn semantic_bootstrap_does_not_initialize_without_postgres() {
        let inference = cih_embed::EmbedInferenceConfig::default();
        let called = Arc::new(AtomicBool::new(false));
        let called_by_initializer = called.clone();
        let initialized = initialize_optional_semantic(None, inference, move |_, _| async move {
            called_by_initializer.store(true, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(())
        })
        .await
        .unwrap();

        assert!(initialized.is_none());
        assert!(!called.load(Ordering::SeqCst));
    }
}
