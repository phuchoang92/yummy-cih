//! Server startup — read config, build the graph store, assemble the axum app
//! (MCP endpoint, graph browser UI, wiki search, health/ready), and serve until
//! shutdown. The tool surface lives in `app.rs`; this is just the wiring.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::{middleware, routing::get};
use cih_embed::{EmbedModelKind, EmbedStore};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::StreamableHttpService;
use tower_http::{compression::CompressionLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::app::CihServer;
use crate::config::{build_store, Config};
use crate::{browser, files, server, wiki};

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
    tracing::info!(?cfg, "starting CIH MCP server");

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
    let wiki_state = wiki::WikiSearchState::new(cfg.graph_key.clone());
    let cih = CihServer::new(
        store.clone(),
        cfg.artifacts_dir.clone(),
        embed_store,
        graph_key,
        cfg.group.clone(),
        cfg.falkor_url.clone(),
        (
            cfg.max_concurrent_queries,
            Duration::from_millis(cfg.query_queue_timeout_ms),
        ),
        files::ReadFileLimits {
            max_bytes: cfg.read_file_max_bytes,
            max_lines: cfg.read_file_max_lines,
        },
        wiki_state.clone(),
    );
    let browser_state = browser::BrowserState::new(
        cih.store.clone(),
        cih.search.clone(),
        cfg.artifacts_dir.clone(),
    );

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
            server::auth_middleware,
        ));

    // Wiki search is fetched by browsers (docs-viewer), so it needs CORS.
    // The CorsLayer must wrap the auth middleware (layers run outermost-last):
    // OPTIONS preflights carry no Authorization header and would otherwise 401
    // whenever CIH_API_TOKEN is set.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([axum::http::Method::GET])
        .allow_headers([axum::http::header::AUTHORIZATION]);
    let wiki_routes = wiki::router(wiki_state)
        .layer(middleware::from_fn_with_state(
            cfg.api_token.clone(),
            server::auth_middleware,
        ))
        .layer(cors);

    let ready_state = (store, cfg.artifacts_dir.clone());
    let public = axum::Router::new()
        .route("/health", get(server::health_handler))
        .route("/ready", get(server::ready_handler).with_state(ready_state));

    let app = public
        .merge(protected)
        .merge(wiki_routes)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(120),
        ));

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("MCP (Streamable HTTP) listening on http://{}/mcp", cfg.bind);
    tracing::info!("CIH graph browser listening on http://{}/graph", cfg.bind);
    tracing::info!("wiki search listening on http://{}/wiki/search", cfg.bind);

    axum::serve(listener, app)
        .with_graceful_shutdown(server::shutdown_signal())
        .await?;
    tracing::info!("server shut down cleanly");
    Ok(())
}
