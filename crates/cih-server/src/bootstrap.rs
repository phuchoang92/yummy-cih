//! Process bootstrap: configuration, dependency assembly, transports, and serving.
//! (MCP endpoint, graph browser UI, wiki search, health/ready), and serve until
//! shutdown. Protocol behavior lives under `transport`; this is composition.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::{middleware, routing::get};
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
use crate::application::files::{FileService, ReadFileLimits};
use crate::application::graph::GraphQueryService;
use crate::application::indexing::IndexingService;
use crate::application::search::SearchService;
use crate::application::taint::TaintService;
use crate::application::testing::TestingService;
use crate::application::wiki_search::{WikiPageService, WikiSearchService};
use crate::config::{build_store, CacheBudgets, Config};
use crate::infrastructure::artifact_repository::{ArtifactCache, ArtifactRepository};
use crate::infrastructure::cross_repo_graph::XflowState;
use crate::infrastructure::local_job_scheduler::{IndexScheduler, RegistryIndexTargetResolver};
use crate::infrastructure::repo_context_provider::DefaultRepoContextProvider;
use crate::infrastructure::search_provider::{SearchCache, SearchState};
use crate::infrastructure::wiki_repository::{
    WikiBundlePageRepository, WikiBundleSearchRepository, WikiOverviewRepository, WikiSearchState,
};
use crate::ports::repo_context_provider::RepoContextProvider;
use crate::transport::http::{browser, health, wiki as wiki_http};
use crate::transport::mcp::CihServer;

#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_services(
    store: Arc<dyn GraphStore>,
    artifacts_dir: Option<std::path::PathBuf>,
    embed_store: Option<Arc<EmbedStore>>,
    graph_key: String,
    group: Option<String>,
    backend: String,
    falkor_url: String,
    store_limits: (usize, Duration),
    read_file_limits: ReadFileLimits,
    wiki_state: WikiSearchState,
) -> Arc<AppServices> {
    let search_cache = SearchCache::from_env();
    let search = SearchState::with_cache(
        artifacts_dir.clone(),
        embed_store.clone(),
        search_cache.clone(),
    );
    let browser_service = GraphBrowserService::new(store.clone(), Arc::new(search.clone()));
    let repo_contexts: Arc<dyn RepoContextProvider> =
        Arc::new(DefaultRepoContextProvider::production(
            graph_key.clone(),
            store,
            search,
            artifacts_dir,
            backend.clone(),
            falkor_url.clone(),
            store_limits,
            embed_store,
            search_cache,
        ));
    let jobs = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let artifacts: Arc<dyn ArtifactRepository> = Arc::new(ArtifactCache::new());
    let index_scheduler = Arc::new(IndexScheduler::new(
        jobs,
        artifacts.clone(),
        backend,
        falkor_url,
    ));
    let indexing_service =
        IndexingService::new(Arc::new(RegistryIndexTargetResolver), index_scheduler);
    let contract_service = ContractService::new(
        repo_contexts.clone(),
        XflowState::new(artifacts.clone()),
        artifacts.clone(),
    );
    let architecture_overview = ArchitectureOverviewService::new(
        repo_contexts.clone(),
        Arc::new(WikiOverviewRepository::new(wiki_state.clone())),
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
    Arc::new(AppServices {
        repos: repos.clone(),
        graph: GraphUseCases {
            queries: GraphQueryService::new(repos.clone(), ChangeDetectionService::new()),
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
            analysis: TestingService::new(repos.clone(), TaintService::new(artifacts)),
        },
        docs: DocsUseCases {
            wiki_search,
            wiki_page,
        },
        files: FileUseCases {
            access: FileService::new(repos.clone(), read_file_limits),
        },
        admin: AdminUseCases {
            repositories: RepositoryAdminService::new(repos.clone(), graph_key, group),
            patterns: ResolvePatternService::new(repos.clone(), indexing_service.clone()),
            indexing: indexing_service,
        },
    })
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

    let cfg = Config::from_env();
    let cache_budgets = CacheBudgets::from_env()?;
    tracing::info!(?cfg, "starting CIH MCP server");
    tracing::info!(
        artifact_cache_bytes = cache_budgets.artifact_bytes,
        wiki_cache_bytes = cache_budgets.wiki_bytes,
        search_cache_bytes = cache_budgets.search_bytes,
        total_cache_bytes = cache_budgets.total_bytes,
        "validated process cache budgets"
    );

    cfg.check_auth_posture()?;
    if cfg.api_token.is_none() {
        tracing::warn!("CIH_API_TOKEN is not set — server is open to unauthenticated requests");
    }
    let store = build_store(&cfg).await?;
    let embed_store = if let Some(pg_url) = &cfg.pg_url {
        let store = EmbedStore::connect(pg_url, EmbedModelKind::MiniLm).await?;
        store.ensure_schema().await?;
        Some(Arc::new(store))
    } else {
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
        ReadFileLimits {
            max_bytes: cfg.read_file_max_bytes,
            max_lines: cfg.read_file_max_lines,
        },
        wiki_state.clone(),
    );
    let cih = CihServer::new(services.clone());
    let browser_state =
        browser::BrowserState::new(services.graph.browser.clone(), cfg.artifacts_dir.clone());
    let wiki_search_service = services.docs.wiki_search.clone();

    let service = StreamableHttpService::new(
        move || Ok(cih.clone()),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );

    let protected = axum::Router::new()
        .nest_service("/mcp", service)
        .merge(browser::router(browser_state))
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
            cfg.api_token.clone(),
            health::auth_middleware,
        ))
        .layer(cors);

    let ready_state = ReadinessService::new(store, cfg.artifacts_dir.clone());
    let public = axum::Router::new()
        .route("/health", get(health::health_handler))
        .route("/ready", get(health::ready_handler).with_state(ready_state));

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
