//! CIH MCP server — Rust `rmcp` + `axum`, Streamable HTTP.
//!
//! Exposes the code-intelligence graph as MCP tools that map 1:1 onto
//! `GraphStore` methods. The graph backend (FalkorDB now, Neptune at go-live)
//! is selected by `CIH_GRAPH_BACKEND` and injected as `Arc<dyn GraphStore>`.
//!
//! ⚠️ rmcp version note: the `#[tool_router]` / `#[tool]` / `ServerHandler`
//! macros and the `StreamableHttpService` constructor shape move between rmcp
//! releases. If `cargo build` flags the wiring below, reconcile it against
//! docs.rs for the version you resolve — the tool BODIES (the `self.store.*`
//! calls) are SDK-agnostic and stay as-is.

mod config;

use std::sync::Arc;

use anyhow::Result;
use cih_core::NodeId;
use cih_graph_store::{Direction, GraphStore, GraphStoreError};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::{build_store, Config};

#[derive(Clone)]
struct CihServer {
    store: Arc<dyn GraphStore>,
    tool_router: ToolRouter<CihServer>,
}

// ---- tool argument schemas ----

#[derive(Debug, Deserialize, JsonSchema)]
struct ContextArgs {
    /// Symbol id (e.g. `Method:com.acme.UserService#save`).
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImpactArgs {
    /// Symbol id to analyze.
    name: String,
    /// `upstream` (callers / blast radius, default), `downstream`, or `both`.
    #[serde(default)]
    direction: Option<String>,
    /// Max traversal depth (default 4).
    #[serde(default)]
    max_depth: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CommunitiesArgs {
    /// Optional maximum number of communities to return.
    #[serde(default)]
    limit: Option<usize>,
}

#[tool_router]
impl CihServer {
    fn new(store: Arc<dyn GraphStore>) -> Self {
        Self {
            store,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "360° context for a symbol: its node, callers, callees, and processes.")]
    async fn context(
        &self,
        Parameters(args): Parameters<ContextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let ctx = self
            .store
            .context(&NodeId::new(args.name))
            .await
            .map_err(to_mcp)?;
        json_result(&ctx)
    }

    #[tool(description = "Blast radius of changing a symbol: affected symbols, depth, and risk.")]
    async fn impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, McpError> {
        let dir = match args.direction.as_deref() {
            Some("downstream") => Direction::Downstream,
            Some("both") => Direction::Both,
            _ => Direction::Upstream,
        };
        let res = self
            .store
            .impact(&NodeId::new(args.name), dir, args.max_depth.unwrap_or(4))
            .await
            .map_err(to_mcp)?;
        json_result(&res)
    }

    #[tool(description = "List community clusters detected in the codebase.")]
    async fn communities(
        &self,
        Parameters(args): Parameters<CommunitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut communities = self.store.communities().await.map_err(to_mcp)?;
        if let Some(limit) = args.limit {
            communities.truncate(limit);
        }
        json_result(&communities)
    }
}

#[tool_handler]
impl ServerHandler for CihServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "cih".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "Code Intelligence Hub — query the call graph: `context`, `impact`, `communities`."
                    .into(),
            ),
        }
    }
}

fn to_mcp(e: GraphStoreError) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content =
        Content::json(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![content]))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cih_server=debug".into()),
        )
        .init();

    let cfg = Config::from_env();
    tracing::info!(?cfg, "starting CIH MCP server");
    let store = build_store(&cfg).await?;
    let server = CihServer::new(store);

    // Streamable HTTP MCP endpoint mounted at /mcp.
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("MCP (Streamable HTTP) listening on http://{}/mcp", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}
