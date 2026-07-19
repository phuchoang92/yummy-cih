//! CIH MCP server core — rmcp + axum wiring, tool definitions, and the
//! [`run`] entry point (the `cih-server` binary is a thin shim around it).
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};
use anyhow::Result;
use cih_embed::EmbedStore;
use cih_graph_store::GraphStore;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Implementation, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParam, ProtocolVersion, ReadResourceRequestParam,
        ReadResourceResult, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};

use crate::args::*;
use crate::jobs::Jobs;
use crate::symbol::{AmbiguousResult, SymbolResolution};
use crate::utils::{json_result, parse_direction, text_result, to_mcp};
use crate::{artifact_cache, changes, feature, files, resources, search, symbol, wiki, xflow};

use crate::search::{QueryArgs, QueryResult, SearchState};

/// Tool handlers split out of this god-module into per-theme `#[tool_router]`
/// impl blocks; each emits a `*_router()` merged in [`CihServer::new`].
mod tools_admin;
mod tools_crossrepo;
mod tools_files;
mod tools_overview;
mod tools_testing;
mod tools_wiki;

#[cfg(test)]
mod dispatch_tests;

/// Map a registry `artifacts_dir` (the versioned `.cih/artifacts/<hash>` dir,
/// which cross-repo readers use directly) to the unversioned parent that
/// `SearchState`/`GraphArtifacts::latest_in_dir` expect for BM25.
fn unversioned_artifacts_dir(versioned: &str) -> PathBuf {
    Path::new(versioned)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(versioned))
}

/// The graph store + BM25 search resolved for one target repo (the primary, or
/// a group member named via a tool's `repo` arg).
struct RepoCtx {
    store: Arc<dyn GraphStore>,
    graph_key: String,
    search: SearchState,
}

#[derive(Clone)]
pub(crate) struct CihServer {
    /// The primary repo's store (also seeded into `stores` under `graph_key`).
    pub(crate) store: Arc<dyn GraphStore>,
    pub(crate) search: SearchState,
    /// Default graph key — the repo served when a tool's `repo` arg is empty.
    graph_key: String,
    /// Home group (`CIH_GROUP`): when set, `list_repos` scopes to its members.
    group: Option<String>,
    /// Graph backend name (`CIH_GRAPH_BACKEND`) — passed to the store factory
    /// for lazily-connected member stores.
    backend: String,
    falkor_url: String,
    /// (max_concurrent_queries, acquire_timeout) for lazily-connected member stores.
    store_limits: (usize, Duration),
    /// Shared pgvector handle, cloned into each member repo's `SearchState`.
    embed_store: Option<Arc<EmbedStore>>,
    /// Lazily-connected graph store per graph key — one server, many graphs.
    stores: Arc<RwLock<HashMap<String, Arc<dyn GraphStore>>>>,
    /// Per-repo BM25 search state, keyed by the unversioned artifacts dir.
    searches: Arc<RwLock<HashMap<String, SearchState>>>,
    jobs: Jobs,
    read_file_limits: files::ReadFileLimits,
    wiki: wiki::WikiSearchState,
    /// Cross-call artifact-graph cache for cross-repo tools (trace_flow_x,
    /// api_impact caller walks).
    xflow: xflow::XflowState,
    /// Cross-call cache of raw parsed artifacts for taint_paths / shape_check,
    /// which reload nodes.jsonl+edges.jsonl on every call.
    artifacts: artifact_cache::ArtifactCache,
    tool_router: ToolRouter<CihServer>,
}

#[tool_router]
impl CihServer {
    #[allow(clippy::too_many_arguments)] // one-time wiring called only from run()
    pub(crate) fn new(
        store: Arc<dyn GraphStore>,
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
        graph_key: String,
        group: Option<String>,
        backend: String,
        falkor_url: String,
        store_limits: (usize, Duration),
        read_file_limits: files::ReadFileLimits,
        wiki: wiki::WikiSearchState,
    ) -> Self {
        let search = SearchState::new(artifacts_dir.clone(), embed_store.clone());
        // Seed the caches with the primary so the default (empty-repo) path
        // reuses the already schema-initialised connection + BM25 state.
        let mut stores = HashMap::new();
        stores.insert(graph_key.clone(), store.clone());
        let mut searches = HashMap::new();
        if let Some(dir) = &artifacts_dir {
            searches.insert(dir.to_string_lossy().into_owned(), search.clone());
        }
        Self {
            store,
            search,
            graph_key,
            group,
            backend,
            falkor_url,
            store_limits,
            embed_store,
            stores: Arc::new(RwLock::new(stores)),
            searches: Arc::new(RwLock::new(searches)),
            jobs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            read_file_limits,
            wiki,
            xflow: xflow::XflowState::new(),
            artifacts: artifact_cache::ArtifactCache::new(),
            tool_router: Self::tool_router()
                + Self::files_router()
                + Self::crossrepo_router()
                + Self::testing_router()
                + Self::wiki_router()
                + Self::overview_router()
                + Self::admin_router(),
        }
    }

    /// Resolve a tool's `repo` arg (empty = the server's primary graph key) to
    /// its graph store + BM25 search, connecting/loading lazily and caching by
    /// graph key / artifacts dir. This is what turns one server into a
    /// whole-group server: every member's graph lives in the same backend, so
    /// a store per key reaches them all.
    async fn resolve(&self, repo: &str) -> Result<RepoCtx, McpError> {
        let entry = crate::utils::resolve_repo_entry(repo, &self.graph_key)
            .map_err(|e| McpError::invalid_params(e, None))?;
        let store = self.store_for(&entry.graph_key).await?;
        let search = self.search_for(&entry.artifacts_dir).await;
        Ok(RepoCtx {
            store,
            graph_key: entry.graph_key,
            search,
        })
    }

    /// Get-or-connect a graph store for `graph_key` (cached across calls).
    /// Single-shot `ensure_schema` (unlike startup's retry loop): per-key
    /// connects happen while serving traffic, where failing fast is correct.
    async fn store_for(&self, graph_key: &str) -> Result<Arc<dyn GraphStore>, McpError> {
        if let Some(store) = self.stores.read().await.get(graph_key) {
            return Ok(store.clone());
        }
        let store = cih_store_factory::connect_store(
            &self.backend,
            &self.falkor_url,
            graph_key,
            &cih_store_factory::StoreOptions {
                query_limit: Some(self.store_limits),
            },
        )
        .map_err(|e| McpError::internal_error(format!("connect graph '{graph_key}': {e}"), None))?;
        store.ensure_schema().await.map_err(|e| {
            McpError::internal_error(format!("schema init for graph '{graph_key}': {e}"), None)
        })?;
        self.stores
            .write()
            .await
            .insert(graph_key.to_string(), store.clone());
        Ok(store)
    }

    /// Get-or-build a `SearchState` for a repo. The registry's `artifacts_dir`
    /// is the *versioned* dir (`.cih/artifacts/<hash>`); BM25 wants the
    /// unversioned parent, which `SearchState`/`latest_in_dir` resolve.
    async fn search_for(&self, versioned_artifacts_dir: &str) -> SearchState {
        let dir = unversioned_artifacts_dir(versioned_artifacts_dir);
        let key = dir.to_string_lossy().into_owned();
        if let Some(search) = self.searches.read().await.get(&key) {
            return search.clone();
        }
        let search = SearchState::new(Some(dir), self.embed_store.clone());
        self.searches.write().await.insert(key, search.clone());
        search
    }

    async fn resolve_symbol(
        &self,
        store: &Arc<dyn GraphStore>,
        name: &str,
    ) -> Result<SymbolResolution, McpError> {
        symbol::resolve_symbol(store, name).await
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
        let rc = self.resolve(&args.repo).await?;
        let id = match self.resolve_symbol(&rc.store, &args.name).await? {
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
        let ctx = rc.store.context(&id).await.map_err(to_mcp)?;
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
        let rc = self.resolve(&args.repo).await?;
        let id = match self.resolve_symbol(&rc.store, &args.name).await? {
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
        let dir = parse_direction(if args.direction.is_empty() {
            None
        } else {
            Some(args.direction.as_str())
        });
        let res = rc
            .store
            .impact(
                &id,
                dir,
                if args.max_depth == 0 {
                    4
                } else {
                    args.max_depth
                },
            )
            .await
            .map_err(to_mcp)?;
        if args.format == "diagram" {
            return json_result(&render_d3_impact(&res));
        }
        json_result(&res)
    }

    #[tool(description = "List community clusters detected in the codebase.")]
    async fn communities(
        &self,
        Parameters(args): Parameters<CommunitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rc = self.resolve(&args.repo).await?;
        let mut communities = rc.store.communities().await.map_err(to_mcp)?;
        if args.limit > 0 {
            communities.truncate(args.limit);
        }
        if args.format == "diagram" {
            let edges = rc.store.community_graph().await.map_err(to_mcp)?;
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
        let rc = self.resolve(&args.repo).await?;
        let limit = search::query_limit(args.limit);
        let hits = rc
            .search
            .query_hits(&args.q, limit)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let subgraph = if args.expand && !hits.is_empty() {
            let seeds: Vec<cih_core::NodeId> =
                hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(rc.store.subgraph(&seeds, 1).await.map_err(to_mcp)?)
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
        let rc = self.resolve(&args.repo).await?;
        let limit = (if args.limit == 0 { 10 } else { args.limit }).clamp(1, 50);
        let hits = rc
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
        let prefix = if args.prefix.is_empty() {
            None
        } else {
            Some(args.prefix.as_str())
        };
        let limit = (if args.limit == 0 { 200 } else { args.limit }).clamp(1, 1000);
        let rc = self.resolve(&args.repo).await?;
        let routes: Vec<cih_graph_store::RouteInfo> =
            rc.store.route_map(prefix, limit).await.map_err(to_mcp)?;
        if args.format == "openapi" {
            return json_result(&render_openapi(&routes));
        }
        json_result(&routes)
    }

    #[tool(description = "List all repos indexed in the CIH registry with their stats.")]
    async fn list_repos(&self, _: Parameters<ListReposArgs>) -> Result<CallToolResult, McpError> {
        let reg = cih_core::Registry::load_cached();
        // Multi-repo serving: when the server fronts a group, scope the listing
        // to its members and flag the primary (the repo used when a tool's
        // `repo` arg is empty). Pass `repo` to any deep tool to target another.
        if let Some(group) = &self.group {
            let groups = cih_core::GroupRegistry::load_cached();
            if let Some(g) = groups.find(group) {
                let members: Vec<&cih_core::RegistryEntry> = reg
                    .entries
                    .iter()
                    .filter(|e| g.repos.iter().any(|r| r == &e.name))
                    .collect();
                return json_result(&serde_json::json!({
                    "group": group,
                    "primary_graph_key": self.graph_key,
                    "repos": members,
                }));
            }
        }
        json_result(&reg.entries)
    }

    #[tool(
        description = "Return registry entry and staleness for one repo (by name or path), \
        plus contract-sync freshness for every group the repo belongs to."
    )]
    async fn status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let reg = cih_core::Registry::load_cached();
        if let Some(entry) = reg.find(&args.name) {
            let stale = reg.is_stale(&args.name);
            let group_registry = cih_core::GroupRegistry::load_cached();
            let groups: Vec<serde_json::Value> = group_registry
                .groups_containing(&entry.name)
                .map(|group| {
                    let state = cih_core::group_dir(&group.name)
                        .and_then(|dir| cih_core::SyncState::load(&dir));
                    let contracts_exist =
                        cih_core::contracts_path(&group.name).is_some_and(|path| path.exists());
                    let group_stale = cih_core::group_contracts_stale(
                        group,
                        &reg,
                        state.as_ref(),
                        contracts_exist,
                    );
                    serde_json::json!({
                        "name": group.name,
                        "contracts_synced_at": state.map(|s| s.synced_at),
                        "stale": group_stale,
                    })
                })
                .collect();
            #[derive(serde::Serialize)]
            struct Out<'a> {
                entry: &'a cih_core::RegistryEntry,
                stale: bool,
                groups: Vec<serde_json::Value>,
            }
            json_result(&Out {
                entry,
                stale,
                groups,
            })
        } else {
            Err(crate::utils::repo_not_found(&args.name))
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
        // Resolve so the graph queried matches the repo being diffed (a
        // non-primary `repo` must hit its own graph, not the primary's).
        let rc = self.resolve(&args.repo).await?;
        changes::detect_changes(&rc.store, &rc.graph_key, args).await
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
        let rc = self.resolve(&args.repo).await?;
        let id = match self.resolve_symbol(&rc.store, &args.entry_point).await? {
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
        let depth = (if args.max_depth == 0 {
            6
        } else {
            args.max_depth
        })
        .clamp(1, 10);
        let steps = rc.store.flow_downstream(&id, depth).await.map_err(to_mcp)?;
        if args.format == "mermaid" {
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
        let limit = if args.limit == 0 {
            20
        } else {
            args.limit.min(500)
        };
        let rc = self.resolve(&args.repo).await?;
        let hotspots = rc
            .store
            .complexity_hotspots(
                if args.min_cyclomatic == 0 {
                    None
                } else {
                    Some(args.min_cyclomatic)
                },
                if args.min_cognitive == 0 {
                    None
                } else {
                    Some(args.min_cognitive)
                },
                if args.min_transitive_loop == 0 {
                    None
                } else {
                    Some(args.min_transitive_loop)
                },
                limit,
            )
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
        let rc = self.resolve(&args.repo).await?;
        let id = match self.resolve_symbol(&rc.store, &args.name).await? {
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
        let min_jaccard = if args.min_jaccard == 0.0 {
            0.95
        } else {
            args.min_jaccard
        };
        let limit = if args.limit == 0 { 10 } else { args.limit };
        let similar = rc
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
        let rc = self.resolve(&args.repo).await?;
        feature::feature_map(&rc.store, &rc.search, args).await
    }
}

/// Whether per-tool timing is logged. Read once from `CIH_TOOL_TIMING`
/// (truthy = `1`/`true`); off by default, so the log is silent unless enabled.
fn tool_timing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("CIH_TOOL_TIMING").ok().as_deref(),
            Some("1") | Some("true")
        )
    })
}

impl ServerHandler for CihServer {
    // NOTE: this `call_tool` is the manual expansion of rmcp 0.7.0's
    // `#[tool_handler]` (build a `ToolCallContext`, dispatch via `tool_router`),
    // wrapped with optional timing. If an rmcp bump changes the macro output,
    // reconcile it here the same way — see the version note at the top of this file.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let timing = tool_timing_enabled();
        // Only pay the string/lookup work when timing is on.
        let started = timing.then(|| {
            let name = request.name.to_string();
            let repo = request
                .arguments
                .as_ref()
                .and_then(|m| m.get("repo"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            (name, repo, std::time::Instant::now())
        });
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;
        if let Some((name, repo, t0)) = started {
            tracing::info!(
                tool = %name,
                repo = repo.as_deref().unwrap_or(""),
                elapsed_ms = t0.elapsed().as_millis() as u64,
                ok = result.is_ok(),
                "tool_call"
            );
        }
        result
    }

    // `#[tool_handler]` generates BOTH `call_tool` and `list_tools`; since we
    // hand-expand `call_tool` (above), we must provide `list_tools` too, or
    // discovery falls back to the trait default (an empty list) and clients that
    // rely on `tools/list` see no tools. This mirrors the macro output verbatim.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

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
                "Code Intelligence Hub (CIH) — structural call-graph intelligence for indexed repositories.\n\
                 \n\
                 ## Always use CIH tools instead of grep/read when possible.\n\
                 \n\
                 ## NodeId format\n\
                 Full form: `Kind:fully.qualified.Name` (e.g. `Class:org.phuc.commerce.order.OrderService`, `Route:POST /api/v1/orders`).\n\
                 Short names (e.g. `OrderService`) also work and trigger interactive disambiguation.\n\
                 \n\
                 ## IMPORTANT: Always call `list_repos()` first to get the exact repo name before calling any other CIH tool.\n\
                 \n\
                 ## Core workflow\n\
                 1. `list_repos` — see what is indexed\n\
                 2. `architecture_overview(repo=...)` — one-call orientation: modules with anchor symbols, route groups, entrypoints, wiki pointers. Start here in an unfamiliar repo; it replaces chaining status/communities/route_map/search_wiki\n\
                 3. `search_code(query=...)` — find symbols by keyword\n\
                 4. `context(name=...)` — callers, callees, which routes reach a symbol\n\
                 5. `impact(name=..., direction=\"upstream\")` — blast radius before changing something; add format=\"diagram\" for a visual\n\
                 6. `trace_flow(name=\"Route:METHOD /path\")` — follow an HTTP request end-to-end; add format=\"mermaid\" for a diagram\n\
                 7. `route_map()` — all HTTP routes; add format=\"openapi\" for an OpenAPI spec\n\
                 8. `communities()` — module/service groupings across the codebase\n\
                 \n\
                 ## Indexing\n\
                 `index_repo(repo_path=\"/abs/path\")` → returns job_id → poll with `index_status(job_id=...)`.\n\
                 \n\
                 ## Wiki\n\
                 `search_wiki(query=..., kind=\"po\"|\"ba\"|\"dev\")` — search the generated role-based docs \
                 (persona pages carry their persona as the kind); \
                 `get_wiki_page(slug=...)` — fetch a page's markdown. \
                 Pages are also readable as `cih://repo/{name}/wiki/{slug}` resources.\n\
                 \n\
                 ## Other tools\n\
                 `feature_map`, `query`, `detect_changes`, `group_contracts`, `api_impact`, `shape_check`,\n\
                 `test_coverage`, `regression_scope`, `untested_paths`, `find_duplicates`, `complexity_hotspots`, `read_file`, `grep_files`.\n\
                 \n\
                 ## Security\n\
                 `taint_paths(category=\"sql\"|\"exec\"|\"file\"|\"html\")` — source→sink flows from HTTP/event entry points; refine=true for flow-sensitive confirmation."
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
#[cfg(test)]
mod tests {
    use super::unversioned_artifacts_dir;
    use super::CihServer;
    use std::path::PathBuf;

    #[test]
    fn split_routers_register_all_tools_without_dropping_any() {
        // Guards the app.rs tool split: the per-theme `#[tool_router]` impls must
        // merge into the same tool surface, with the moved tools still present.
        // Keep the router list here in sync with `CihServer::new`.
        let router = CihServer::tool_router()
            + CihServer::files_router()
            + CihServer::crossrepo_router()
            + CihServer::testing_router()
            + CihServer::wiki_router()
            + CihServer::overview_router()
            + CihServer::admin_router();
        assert_eq!(
            router.list_all().len(),
            30,
            "tool count changed after the split — a tool was dropped or duplicated"
        );
        for tool in [
            "read_file",
            "grep_files",
            "group_contracts",
            "taint_paths",
            "search_wiki",
            "index_repo",
            "impact",
            "architecture_overview",
        ] {
            assert!(router.has_route(tool), "missing tool after split: {tool}");
        }
        // Every tool an architecture_overview `next` hint can emit must be a
        // real route — a hint that drifts from the tool surface teaches clients
        // hallucinated calls.
        for tool in crate::overview::HINT_TOOLS {
            assert!(
                router.has_route(tool),
                "overview next-hint references unregistered tool: {tool}"
            );
        }
    }

    #[test]
    fn versioned_artifacts_dir_maps_to_unversioned_parent() {
        // Registry stores the versioned dir; BM25 search needs the parent.
        assert_eq!(
            unversioned_artifacts_dir("/repo/.cih/artifacts/b5bb9fb09e9b7a16"),
            PathBuf::from("/repo/.cih/artifacts")
        );
        // Distinct repos collapse to distinct search dirs (cache keys).
        assert_ne!(
            unversioned_artifacts_dir("/a/.cih/artifacts/deadbeef"),
            unversioned_artifacts_dir("/b/.cih/artifacts/deadbeef")
        );
        // Degenerate input (no parent) falls back to itself rather than panicking.
        assert_eq!(unversioned_artifacts_dir("/"), PathBuf::from("/"));
    }
}
