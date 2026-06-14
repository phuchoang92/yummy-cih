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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use cih_core::{GraphArtifacts, NodeId, VersionId};
use cih_embed::{EmbedModelKind, EmbedStore, SemanticHit};
use cih_graph_store::{Direction, GraphStore, GraphStoreError, Subgraph};
use cih_search::{rrf_merge, SearchHit, SearchIndex};
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
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{build_store, Config};

#[derive(Clone)]
struct CihServer {
    store: Arc<dyn GraphStore>,
    bm25: Arc<RwLock<Option<SearchIndex>>>,
    embed_store: Option<Arc<EmbedStore>>,
    artifacts_dir: Option<PathBuf>,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryArgs {
    /// Natural language or symbol keyword query.
    q: String,
    /// Maximum number of fused hits to return (default 10).
    #[serde(default)]
    limit: Option<usize>,
    /// Include a one-hop subgraph around the top results.
    #[serde(default)]
    expand: Option<bool>,
}

#[derive(Debug, Serialize)]
struct QueryResult {
    hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subgraph: Option<Subgraph>,
}

#[tool_router]
impl CihServer {
    fn new(
        store: Arc<dyn GraphStore>,
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
    ) -> Self {
        Self {
            store,
            bm25: Arc::new(RwLock::new(None)),
            embed_store,
            artifacts_dir,
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

    #[tool(
        description = "Hybrid search over code symbols using BM25 and optional semantic embeddings."
    )]
    async fn query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        if self.artifacts_dir.is_none() && self.embed_store.is_none() {
            return Err(McpError::internal_error(
                "query unavailable: set CIH_ARTIFACTS_DIR for BM25 and/or CIH_PG_URL for semantic search",
                None,
            ));
        }

        let lexical = if let Some(index) = self.bm25_index().await? {
            index.search(&args.q, limit * 2)
        } else {
            Vec::new()
        };
        let semantic = if let Some(embed_store) = &self.embed_store {
            embed_store
                .semantic_search(&args.q, limit * 2, 0.5)
                .await
                .map_err(|err| McpError::internal_error(err.to_string(), None))?
                .into_iter()
                .map(semantic_to_search_hit)
                .collect()
        } else {
            Vec::new()
        };

        let hits = rrf_merge(lexical, semantic, limit);
        let subgraph = if args.expand.unwrap_or(false) && !hits.is_empty() {
            let seeds: Vec<NodeId> = hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(self.store.subgraph(&seeds, 1).await.map_err(to_mcp)?)
        } else {
            None
        };

        json_result(&QueryResult { hits, subgraph })
    }
}

impl CihServer {
    async fn bm25_index(&self) -> Result<Option<SearchIndex>, McpError> {
        let Some(artifacts_dir) = &self.artifacts_dir else {
            return Ok(None);
        };
        {
            let guard = self.bm25.read().await;
            if let Some(index) = guard.as_ref() {
                return Ok(Some(index.clone()));
            }
        }

        let artifacts = latest_graph_artifacts_in_dir(artifacts_dir)
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let nodes = artifacts
            .read_nodes()
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let index = cih_search::build(&nodes);
        let mut guard = self.bm25.write().await;
        *guard = Some(index.clone());
        Ok(Some(index))
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
                "Code Intelligence Hub — query the call graph: `query`, `context`, `impact`, `communities`."
                    .into(),
            ),
        }
    }
}

fn semantic_to_search_hit(hit: SemanticHit) -> SearchHit {
    SearchHit::from_parts(
        hit.node_id,
        hit.kind,
        hit.name,
        None,
        hit.file,
        hit.range,
        hit.score,
        "semantic",
    )
}

fn latest_graph_artifacts_in_dir(parent: &Path) -> anyhow::Result<GraphArtifacts> {
    let entries = std::fs::read_dir(parent)
        .map_err(|err| anyhow::anyhow!("no graph artifacts at {}: {err}", parent.display()))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let nodes_path = dir.join("nodes.jsonl");
        let edges_path = dir.join("edges.jsonl");
        if !nodes_path.is_file() || !edges_path.is_file() {
            continue;
        }
        let version = entry.file_name().to_string_lossy().into_owned();
        let modified = std::fs::metadata(&nodes_path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((
            modified,
            GraphArtifacts {
                nodes_path,
                edges_path,
                version: VersionId(version),
            },
        ));
    }
    candidates.sort_by(|(left_mtime, left), (right_mtime, right)| {
        right_mtime
            .cmp(left_mtime)
            .then_with(|| right.version.0.cmp(&left.version.0))
    });
    candidates
        .into_iter()
        .next()
        .map(|(_, artifacts)| artifacts)
        .ok_or_else(|| anyhow::anyhow!("no complete graph artifacts under {}", parent.display()))
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
    let embed_store = if let Some(pg_url) = &cfg.pg_url {
        let store = EmbedStore::connect(pg_url, EmbedModelKind::MiniLm).await?;
        store.ensure_schema().await?;
        Some(Arc::new(store))
    } else {
        None
    };
    let server = CihServer::new(store, cfg.artifacts_dir.clone(), embed_store);

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
