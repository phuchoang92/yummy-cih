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

mod agent;
mod args;
mod browser;
mod changes;
mod config;
mod contracts;
mod coverage;
mod feature;
mod files;
mod indexing;
mod jobs;
mod layout;
mod resources;
mod search;
mod server;
mod symbol;
mod utils;
mod viz;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{middleware, routing::get};
use cih_embed::{EmbedModelKind, EmbedStore};
use cih_graph_store::GraphStore;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Implementation, ListResourceTemplatesResult, ListResourcesResult,
        PaginatedRequestParam, ProtocolVersion, ReadResourceRequestParam, ReadResourceResult,
        ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpService,
    },
    ErrorData as McpError, RoleServer, ServerHandler,
};
use tower_http::{compression::CompressionLayer, timeout::TimeoutLayer, trace::TraceLayer};
use viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};

use args::*;
use jobs::Jobs;
use symbol::{AmbiguousResult, SymbolResolution};
use utils::{json_result, parse_direction, text_result, to_mcp};

use crate::config::{build_store, Config};
use crate::search::{QueryArgs, QueryResult, SearchState};

#[derive(Clone)]
struct CihServer {
    store: Arc<dyn GraphStore>,
    search: SearchState,
    graph_key: String,
    falkor_url: String,
    jobs: Jobs,
    tool_router: ToolRouter<CihServer>,
    agent: Option<agent::AgentRunner>,
}

#[tool_router]
impl CihServer {
    fn new(
        store: Arc<dyn GraphStore>,
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
        graph_key: String,
        falkor_url: String,
        agent: Option<agent::AgentRunner>,
    ) -> Self {
        Self {
            store,
            search: SearchState::new(artifacts_dir, embed_store),
            graph_key,
            falkor_url,
            jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            tool_router: Self::tool_router(),
            agent,
        }
    }

    async fn resolve_symbol(&self, name: &str) -> Result<SymbolResolution, McpError> {
        symbol::resolve_symbol(&self.store, name).await
    }

    #[tool(
        description = "360° context for a symbol: its node, callers, callees, and processes. \
        Pass a full NodeId (e.g. `Class:com.acme.OrderService`) or a short name; \
        short names return {\"status\":\"ambiguous\",\"candidates\":[...]} when multiple match."
    )]
    async fn context(
        &self,
        Parameters(args): Parameters<ContextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match self.resolve_symbol(&args.name).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(candidates) => {
                return json_result(&AmbiguousResult::from_nodes(candidates));
            }
            SymbolResolution::NotFound => {
                return Err(McpError::invalid_params(
                    format!("symbol '{}' not found", args.name),
                    None,
                ));
            }
        };
        let ctx = self.store.context(&id).await.map_err(to_mcp)?;
        json_result(&ctx)
    }

    #[tool(
        description = "Blast radius of changing a symbol: affected symbols, depth, and risk. \
        Pass a full NodeId or short name; short names that match multiple symbols return \
        {\"status\":\"ambiguous\",\"candidates\":[...]}."
    )]
    async fn impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match self.resolve_symbol(&args.name).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(candidates) => {
                return json_result(&AmbiguousResult::from_nodes(candidates));
            }
            SymbolResolution::NotFound => {
                return Err(McpError::invalid_params(
                    format!("symbol '{}' not found", args.name),
                    None,
                ));
            }
        };
        let dir = parse_direction(args.direction.as_deref());
        let res = self
            .store
            .impact(&id, dir, args.max_depth.unwrap_or(4))
            .await
            .map_err(to_mcp)?;
        if args.format.as_deref() == Some("diagram") {
            return json_result(&render_d3_impact(&res));
        }
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
        if args.format.as_deref() == Some("diagram") {
            let edges = self.store.community_graph().await.map_err(to_mcp)?;
            return json_result(&render_community_diagram(&communities, &edges));
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
        let limit = search::query_limit(args.limit);
        let hits = self
            .search
            .query_hits(&args.q, limit)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let subgraph = if args.expand.unwrap_or(false) && !hits.is_empty() {
            let seeds: Vec<cih_core::NodeId> =
                hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(self.store.subgraph(&seeds, 1).await.map_err(to_mcp)?)
        } else {
            None
        };
        json_result(&QueryResult { hits, subgraph })
    }

    #[tool(
        description = "Search for code by natural language or keywords. Returns ranked code matches \
            with node ID, kind, name, file, and line number. Uses BM25 + semantic (RRF fusion). \
            Use this when you need to find where a concept, feature, or business capability is \
            implemented. Example: search_code(query='rate limiting', limit=10)"
    )]
    async fn search_code(
        &self,
        Parameters(args): Parameters<SearchCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(10).clamp(1, 50);
        let hits = self
            .search
            .query_hits(&args.query, limit)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let matches: Vec<CodeMatch> = hits
            .into_iter()
            .map(|h| CodeMatch {
                node_id: h.node_id.to_string(),
                kind: h.kind.label().to_string(),
                name: h.name,
                qualified_name: h.qualified_name,
                file: h.file,
                line: h.range.start_line,
                score: h.score,
                rank: h.rank as u32,
            })
            .collect();
        json_result(&matches)
    }

    #[tool(
        description = "Ask a natural language question about the codebase and get a grounded answer. \
            The agent calls search_code, get_context, and trace_impact autonomously to build its answer. \
            Requires CIH_AGENT_API_KEY or a supported LLM API key env var (GEMINI_API_KEY, OPENAI_API_KEY). \
            Example: ask_codebase(question='What does POST /orders do end-to-end?')"
    )]
    async fn ask_codebase(
        &self,
        Parameters(args): Parameters<AskCodebaseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let runner = self.agent.as_ref().ok_or_else(|| {
            McpError::internal_error(
                "agent not configured — set CIH_AGENT_API_KEY (or GEMINI_API_KEY / OPENAI_API_KEY)",
                None,
            )
        })?;
        let description = args.codebase_description.as_deref().unwrap_or("a backend codebase");
        let answer = runner
            .ask(&args.question, description)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        json_result(&answer)
    }

    #[tool(
        description = "List HTTP/REST endpoints discovered in the indexed repo. \
        Supported frameworks: Spring MVC/Boot (@GetMapping etc.), NestJS (@Get/@Controller), \
        Flask (@app.route, @bp.route, Blueprint shorthand), FastAPI (@router.get etc. with APIRouter prefix), \
        Express (router.get/post/put/delete/patch). \
        V1 limitation: cross-file router mounts (include_router/register_blueprint with prefix in the \
        app entry-point) are not resolved — routes show their per-file prefix only. \
        Use prefix to filter by path prefix (e.g. prefix=\"/api/users\")."
    )]
    async fn route_map(
        &self,
        Parameters(args): Parameters<RouteMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        let prefix = if args.prefix.is_empty() { None } else { Some(args.prefix.as_str()) };
        let limit = args.limit.unwrap_or(200).clamp(1, 1000);
        let routes: Vec<cih_graph_store::RouteInfo> =
            self.store.route_map(prefix, limit).await.map_err(to_mcp)?;
        if args.format.as_deref() == Some("openapi") {
            return json_result(&render_openapi(&routes));
        }
        json_result(&routes)
    }

    #[tool(description = "List all repos indexed in the CIH registry with their stats.")]
    async fn list_repos(&self, _: Parameters<ListReposArgs>) -> Result<CallToolResult, McpError> {
        let reg = cih_core::Registry::load();
        json_result(&reg.entries)
    }

    #[tool(description = "Return registry entry and staleness for one repo (by name or path).")]
    async fn status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let reg = cih_core::Registry::load();
        if let Some(entry) = reg.find(&args.name) {
            let stale = reg.is_stale(&args.name);
            #[derive(serde::Serialize)]
            struct Out<'a> {
                entry: &'a cih_core::RegistryEntry,
                stale: bool,
            }
            json_result(&Out { entry, stale })
        } else {
            Err(McpError::invalid_params(
                format!("repo '{}' not in registry", args.name),
                None,
            ))
        }
    }

    #[tool(
        description = "Diff-driven change-impact analysis. Runs `git diff` in the repo, \
        maps changed files to graph nodes, traces upstream blast radius via BFS, and \
        scores overall risk (low/medium/high/critical). \
        scope: `working` (all uncommitted), `staged` (index only), `base_ref` (vs a branch/SHA)."
    )]
    async fn detect_changes(
        &self,
        Parameters(args): Parameters<DetectChangesArgs>,
    ) -> Result<CallToolResult, McpError> {
        changes::detect_changes(&self.store, &self.graph_key, args).await
    }

    #[tool(
        description = "Return cross-service contract matches for a repo group. \
        Run `cih-engine group sync <group>` first. Optional kind filter: \
        all, http/http_route, kafka/kafka_topic, spring/spring_event."
    )]
    async fn group_contracts(
        &self,
        Parameters(args): Parameters<GroupContractsArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::group_contracts(args).await
    }

    #[tool(
        description = "Return all services that consume a given HTTP route across a repo group. \
        Path variables ({id}, :id) are normalized to wildcards for matching. \
        Run `cih-engine group sync <group>` first."
    )]
    async fn api_impact(
        &self,
        Parameters(args): Parameters<ApiImpactArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::api_impact(args).await
    }

    #[tool(
        description = "Compare provider HTTP handler response DTO fields against consumer \
        field accesses for all shared HTTP contracts between two repos. \
        Re-run `cih-engine analyze` on both repos (to populate returnType), \
        then `cih-engine group sync <group>` before calling this."
    )]
    async fn shape_check(
        &self,
        Parameters(args): Parameters<ShapeCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::shape_check(args).await
    }

    #[tool(
        description = "Trace the downstream execution chain from an HTTP route or method: \
            controller → services → repos → external HTTP calls and events. \
            Traverses CALLS, HANDLES_ROUTE, EXTERNAL_CALL, PUBLISHES_EVENT, LISTENS_TO edges. \
            Pass a Route node id (e.g. `Route:GET /api/checkout`) or a method id."
    )]
    async fn trace_flow(
        &self,
        Parameters(args): Parameters<TraceFlowArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match self.resolve_symbol(&args.entry_point).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(candidates) => {
                return json_result(&AmbiguousResult::from_nodes(candidates));
            }
            SymbolResolution::NotFound => {
                return Err(McpError::invalid_params(
                    format!("symbol '{}' not found", args.entry_point),
                    None,
                ));
            }
        };
        let depth = args.max_depth.unwrap_or(6).clamp(1, 10);
        let steps = self.store.flow_downstream(&id, depth).await.map_err(to_mcp)?;
        if args.format.as_deref() == Some("mermaid") {
            return text_result(render_mermaid_flow(&id, &steps));
        }
        json_result(&serde_json::json!({
            "entry_point": id.as_str(),
            "depth_limit": depth,
            "step_count": steps.len(),
            "steps": steps,
        }))
    }

    #[tool(
        description = "Return methods with high complexity. Finds methods that are hard to test \
            or maintain: filters by cyclomatic complexity (branches), cognitive complexity \
            (readability penalty), and transitive loop depth (additive nesting through call chain). \
            Use to prioritize refactoring targets."
    )]
    async fn complexity_hotspots(
        &self,
        Parameters(args): Parameters<ComplexityHotspotsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(20);
        let hotspots = self
            .store
            .complexity_hotspots(args.min_cyclomatic, args.min_cognitive, args.min_transitive_loop, limit)
            .await
            .map_err(to_mcp)?;
        json_result(&serde_json::json!({
            "count": hotspots.len(),
            "hotspots": hotspots,
        }))
    }

    #[tool(
        description = "Find near-duplicate methods (MinHash similarity >= threshold). \
            Identifies copy-paste code across the codebase. Returns candidates with Jaccard \
            similarity score and file path. Use to detect inconsistently-maintained duplicates."
    )]
    async fn find_duplicates(
        &self,
        Parameters(args): Parameters<FindDuplicatesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match self.resolve_symbol(&args.name).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(candidates) => {
                return json_result(&AmbiguousResult::from_nodes(candidates));
            }
            SymbolResolution::NotFound => {
                return Err(McpError::invalid_params(
                    format!("symbol '{}' not found", args.name),
                    None,
                ));
            }
        };
        let min_jaccard = args.min_jaccard.unwrap_or(0.95);
        let limit = args.limit.unwrap_or(10);
        let similar = self
            .store
            .similar_methods(&id, min_jaccard, limit)
            .await
            .map_err(to_mcp)?;
        json_result(&serde_json::json!({
            "query_id": id.as_str(),
            "min_jaccard": min_jaccard,
            "count": similar.len(),
            "similar": similar,
        }))
    }

    #[tool(
        description = "Map business keywords to code clusters: BM25 search results grouped \
            by community. Helps answer 'what code implements the checkout feature?' or \
            'which modules handle payments?'"
    )]
    async fn feature_map(
        &self,
        Parameters(args): Parameters<FeatureMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        feature::feature_map(&self.store, &self.search, args).await
    }

    #[tool(
        description = "Return all test methods/classes that cover a symbol (via TESTS edges). \
            Helps a tester understand which tests exercise a given class or method."
    )]
    async fn test_coverage(
        &self,
        Parameters(args): Parameters<TestCoverageArgs>,
    ) -> Result<CallToolResult, McpError> {
        coverage::test_coverage(&self.store, args).await
    }

    #[tool(
        description = "Given a list of changed files, return all test classes that must be \
            re-run. Follows TESTS edges (direct + one-hop via CALLS). Use after `git diff \
            --name-only` to find the regression scope."
    )]
    async fn regression_scope(
        &self,
        Parameters(args): Parameters<RegressionScopeArgs>,
    ) -> Result<CallToolResult, McpError> {
        coverage::regression_scope(&self.store, args).await
    }

    #[tool(
        description = "Return production symbols (classes, methods) under a path prefix that \
            have no test coverage (no inbound TESTS edge). Helps identify coverage gaps."
    )]
    async fn untested_paths(
        &self,
        Parameters(args): Parameters<UntestedPathsArgs>,
    ) -> Result<CallToolResult, McpError> {
        coverage::untested_paths(&self.store, args).await
    }

    #[tool(
        description = "Index a repository so its code graph becomes queryable by the other tools. \
            Runs scan → parse → resolve → load into the live FalkorDB graph. \
            Returns immediately with a `job_id`; use index_status(job_id=...) to poll for completion. \
            Typical time: 5–120 seconds depending on repo size. \
            Example: index_repo(repo_path='/home/user/my-service')"
    )]
    async fn index_repo(
        &self,
        Parameters(args): Parameters<IndexRepoArgs>,
    ) -> Result<CallToolResult, McpError> {
        indexing::index_repo(&self.falkor_url, &self.graph_key, &self.jobs, args).await
    }

    #[tool(
        description = "Poll the status of a repo-indexing job started by index_repo. \
            Returns status (running/done/failed), timing, and output or error message."
    )]
    async fn index_status(
        &self,
        Parameters(args): Parameters<IndexStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let jobs = self.jobs.read().await;
        match jobs.get(&args.job_id) {
            Some(state) => json_result(state),
            None => Err(McpError::invalid_params(
                format!("unknown job_id '{}' — use index_repo to start a job", args.job_id),
                None,
            )),
        }
    }

    #[tool(
        description = "Read the source of a file in the repo. Use the `file` field from \
            search_code or context results as the `path`. Optionally slice with start_line / \
            end_line (1-based, inclusive) to fetch only the relevant section."
    )]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        files::read_file(&self.graph_key, args).await
    }
}

#[tool_handler]
impl ServerHandler for CihServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "cih".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "Code Intelligence Hub — index and query the call graph of any repository. \
                 Indexing: `index_repo` (start), `index_status` (poll). \
                 Graph queries: `context`, `impact`, `trace_flow`, `route_map`, `communities`, \
                 `feature_map`, `search_code`, `query`, `ask_codebase`. \
                 Multi-repo: `group_contracts`, `api_impact`, `shape_check`. \
                 Registry: `list_repos`, `status`. \
                 Change analysis: `detect_changes`. \
                 Test coverage: `test_coverage`, `regression_scope`, `untested_paths`. \
                 Source: `read_file`. \
                 Short symbol names trigger disambiguation; full NodeIds (Kind:fqn) skip it. \
                 Viz: impact(format=\"diagram\"), trace_flow(format=\"mermaid\"), \
                 communities(format=\"diagram\"), route_map(format=\"openapi\")."
                    .into(),
            ),
        }
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        resources::list_resources(request)
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        resources::list_resource_templates(request)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        resources::read_resource(request)
    }
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

    if cfg.api_token.is_none() {
        tracing::warn!("CIH_API_TOKEN is not set — server is open to unauthenticated requests");
    }
    if cfg.agent_api_key.is_none() {
        tracing::info!("no agent API key set — ask_codebase tool will be disabled");
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
    let agent = cfg.agent_api_key.as_deref().map(|key| {
        agent::AgentRunner::new(
            SearchState::new(cfg.artifacts_dir.clone(), None),
            store.clone(),
            cfg.agent_llm_base_url.clone(),
            key.to_string(),
            cfg.agent_llm_model.clone(),
        )
    });
    let cih = CihServer::new(
        store.clone(),
        cfg.artifacts_dir.clone(),
        embed_store,
        graph_key,
        cfg.falkor_url.clone(),
        agent,
    );
    let browser_state = browser::BrowserState::new(cih.store.clone(), cih.search.clone());

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

    let ready_state = (store, cfg.artifacts_dir.clone());
    let public = axum::Router::new()
        .route("/health", get(server::health_handler))
        .route("/ready", get(server::ready_handler).with_state(ready_state));

    let app = public
        .merge(protected)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(120)));

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("MCP (Streamable HTTP) listening on http://{}/mcp", cfg.bind);
    tracing::info!("CIH graph browser listening on http://{}/graph", cfg.bind);

    axum::serve(listener, app)
        .with_graceful_shutdown(server::shutdown_signal())
        .await?;
    tracing::info!("server shut down cleanly");
    Ok(())
}
