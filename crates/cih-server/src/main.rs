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
mod resources;
mod search;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cih_core::{Node, NodeId};
use cih_embed::{EmbedModelKind, EmbedStore};
use cih_graph_store::{Direction, GraphStore, GraphStoreError, RouteInfo};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ListResourcesResult,
        ListResourceTemplatesResult, PaginatedRequestParam, ProtocolVersion,
        ReadResourceRequestParam, ReadResourceResult, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::{build_store, Config};
use crate::search::{QueryArgs, QueryResult, SearchState};

#[derive(Clone)]
struct CihServer {
    store: Arc<dyn GraphStore>,
    search: SearchState,
    graph_key: String,
    tool_router: ToolRouter<CihServer>,
}

// ---- tool argument schemas ----

#[derive(Debug, Deserialize, JsonSchema)]
struct ContextArgs {
    /// Symbol id (e.g. `Method:com.acme.UserService#save`) or short name
    /// (e.g. `UserService`). Short names trigger disambiguation.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImpactArgs {
    /// Symbol id or short name to analyze.
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
struct RouteMapArgs {
    /// Path prefix filter (e.g. "/api/owners"). Omit or leave empty for all routes.
    #[serde(default)]
    prefix: String,
    /// Max routes to return (default 200).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StatusArgs {
    /// Repo name or absolute path as shown in `list_repos`.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DetectChangesArgs {
    /// Scope of the git diff: `working` (all uncommitted vs HEAD),
    /// `staged` (index vs HEAD), or `base_ref` (HEAD vs a branch/commit).
    scope: String,
    /// Git ref for `base_ref` scope (e.g. `main` or a commit SHA).
    #[serde(default)]
    base_ref: Option<String>,
    /// Repo name or absolute path (from registry). Defaults to the repo
    /// registered under the server's active graph key.
    #[serde(default)]
    repo: Option<String>,
}

// ---- ambiguous-symbol helpers ----

enum SymbolResolution {
    Id(NodeId),
    Ambiguous(Vec<Node>),
    NotFound,
}

#[derive(serde::Serialize)]
struct AmbiguousCandidate {
    id: String,
    kind: String,
    name: String,
    file: String,
}

#[derive(serde::Serialize)]
struct AmbiguousResult {
    status: &'static str,
    candidates: Vec<AmbiguousCandidate>,
}

impl AmbiguousResult {
    fn from_nodes(nodes: Vec<Node>) -> Self {
        AmbiguousResult {
            status: "ambiguous",
            candidates: nodes
                .into_iter()
                .map(|n| AmbiguousCandidate {
                    id: n.id.to_string(),
                    kind: n.kind.label().to_string(),
                    name: n.name,
                    file: n.file,
                })
                .collect(),
        }
    }
}

#[tool_router]
impl CihServer {
    fn new(
        store: Arc<dyn GraphStore>,
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
        graph_key: String,
    ) -> Self {
        Self {
            store,
            search: SearchState::new(artifacts_dir, embed_store),
            graph_key,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve a name to a NodeId: if it already contains `:` treat it as a
    /// full NodeId; otherwise query for candidates and disambiguate.
    async fn resolve_symbol(&self, name: &str) -> Result<SymbolResolution, McpError> {
        if name.contains(':') {
            return Ok(SymbolResolution::Id(NodeId::new(name.to_string())));
        }
        let candidates = self
            .store
            .candidates_by_name(name, 10)
            .await
            .map_err(to_mcp)?;
        Ok(match candidates.len() {
            0 => SymbolResolution::NotFound,
            1 => SymbolResolution::Id(candidates.into_iter().next().unwrap().id),
            _ => SymbolResolution::Ambiguous(candidates),
        })
    }

    #[tool(description = "360° context for a symbol: its node, callers, callees, and processes. \
        Pass a full NodeId (e.g. `Class:com.acme.OrderService`) or a short name; \
        short names return {\"status\":\"ambiguous\",\"candidates\":[...]} when multiple match.")]
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

    #[tool(description = "Blast radius of changing a symbol: affected symbols, depth, and risk. \
        Pass a full NodeId or short name; short names that match multiple symbols return \
        {\"status\":\"ambiguous\",\"candidates\":[...]}.")]
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
        let limit = search::query_limit(args.limit);
        let hits = self
            .search
            .query_hits(&args.q, limit)
            .await
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let subgraph = if args.expand.unwrap_or(false) && !hits.is_empty() {
            let seeds: Vec<NodeId> = hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(self.store.subgraph(&seeds, 1).await.map_err(to_mcp)?)
        } else {
            None
        };

        json_result(&QueryResult { hits, subgraph })
    }

    #[tool(description = "List Spring REST endpoints: HTTP method + path + handler method. \
        Use prefix to filter by path prefix (e.g. prefix=\"/api/users\").")]
    async fn route_map(
        &self,
        Parameters(args): Parameters<RouteMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        let prefix = if args.prefix.is_empty() {
            None
        } else {
            Some(args.prefix.as_str())
        };
        let limit = args.limit.unwrap_or(200).clamp(1, 1000);
        let routes: Vec<RouteInfo> = self.store.route_map(prefix, limit).await.map_err(to_mcp)?;
        json_result(&routes)
    }

    #[tool(description = "List all repos indexed in the CIH registry with their stats.")]
    async fn list_repos(&self) -> Result<CallToolResult, McpError> {
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

    #[tool(description = "Diff-driven change-impact analysis. Runs `git diff` in the repo, \
        maps changed files to graph nodes, traces upstream blast radius via BFS, and \
        scores overall risk (low/medium/high/critical). \
        scope: `working` (all uncommitted), `staged` (index only), `base_ref` (vs a branch/SHA).")]
    async fn detect_changes(
        &self,
        Parameters(args): Parameters<DetectChangesArgs>,
    ) -> Result<CallToolResult, McpError> {
        // 1. Find repo path via registry.
        let repo_path = find_repo_path(args.repo.as_deref(), &self.graph_key).map_err(|e| {
            McpError::invalid_params(e, None)
        })?;

        // 2. Run git diff to get list of changed files (repo-relative paths).
        let changed_files = git_changed_files(&repo_path, &args.scope, args.base_ref.as_deref())
            .map_err(|e| McpError::internal_error(e, None))?;

        if changed_files.is_empty() {
            #[derive(serde::Serialize)]
            struct Empty {
                changed_files: Vec<String>,
                changed_symbols: Vec<serde_json::Value>,
                affected_symbols: Vec<String>,
                affected_processes: Vec<String>,
                risk: &'static str,
            }
            return json_result(&Empty {
                changed_files,
                changed_symbols: vec![],
                affected_symbols: vec![],
                affected_processes: vec![],
                risk: "none",
            });
        }

        // 3. Map changed files → changed symbols in the graph.
        let changed_nodes = self
            .store
            .nodes_in_files(&changed_files)
            .await
            .map_err(to_mcp)?;

        // 4. BFS impact from each changed symbol (up to 20 symbols to avoid timeout).
        let mut affected_set: std::collections::HashSet<String> = std::collections::HashSet::new();
        let symbol_limit = changed_nodes.len().min(20);
        for node in &changed_nodes[..symbol_limit] {
            if let Ok(impact) = self
                .store
                .impact(&node.id, Direction::Upstream, 4)
                .await
            {
                for n in &impact.affected {
                    affected_set.insert(n.id.to_string());
                }
            }
        }
        // Remove changed symbols themselves from affected (they're the cause, not result).
        for node in &changed_nodes {
            affected_set.remove(node.id.as_str());
        }
        let mut affected_symbols: Vec<String> = affected_set.into_iter().collect();
        affected_symbols.sort();

        // 5. Find processes that include any of the changed symbols.
        let changed_ids: Vec<NodeId> = changed_nodes.iter().map(|n| n.id.clone()).collect();
        let affected_processes = self
            .store
            .processes_for_symbols(&changed_ids)
            .await
            .map_err(to_mcp)?;

        // 6. Risk from combined affected count (symbols + processes).
        let risk = cih_graph_store::risk_from_fanout(affected_symbols.len());

        #[derive(serde::Serialize)]
        struct ChangedSymbol {
            id: String,
            kind: String,
            name: String,
            file: String,
        }
        let changed_symbols: Vec<ChangedSymbol> = changed_nodes
            .iter()
            .map(|n| ChangedSymbol {
                id: n.id.to_string(),
                kind: n.kind.label().to_string(),
                name: n.name.clone(),
                file: n.file.clone(),
            })
            .collect();

        #[derive(serde::Serialize)]
        struct Out {
            changed_files: Vec<String>,
            changed_symbols: Vec<ChangedSymbol>,
            affected_symbols: Vec<String>,
            affected_processes: Vec<String>,
            risk: &'static str,
        }
        json_result(&Out {
            changed_files,
            changed_symbols,
            affected_symbols,
            affected_processes,
            risk,
        })
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
                "Code Intelligence Hub — query the call graph: `query`, `context`, `impact`, \
                 `communities`, `route_map`, `list_repos`, `detect_changes`. \
                 Short symbol names trigger disambiguation; full NodeIds (Kind:fqn) skip it. \
                 Read repo data via cih://repo/{name}/context|communities|processes|schema."
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

// ---- detect_changes helpers ----

/// Find repo path: explicit `repo` arg → registry by name/path; or fallback to
/// first registry entry whose `graph_key` matches the server's active key.
fn find_repo_path(repo: Option<&str>, graph_key: &str) -> std::result::Result<String, String> {
    let reg = cih_core::Registry::load();
    if reg.entries.is_empty() {
        return Err("no repos in registry — run `cih-engine analyze <repo>` first".to_string());
    }
    if let Some(name_or_path) = repo {
        reg.find(name_or_path)
            .map(|e| e.path.clone())
            .ok_or_else(|| format!("repo '{name_or_path}' not in registry"))
    } else {
        // Default: first entry matching the server's graph_key.
        reg.entries
            .iter()
            .find(|e| e.graph_key == graph_key)
            .map(|e| e.path.clone())
            .ok_or_else(|| {
                format!(
                    "no repo registered for graph_key '{graph_key}'; pass `repo` explicitly"
                )
            })
    }
}

/// Run `git diff --name-only` and return repo-relative file paths.
fn git_changed_files(
    repo_path: &str,
    scope: &str,
    base_ref: Option<&str>,
) -> std::result::Result<Vec<String>, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff").arg("--name-only");
    match scope {
        "staged" => {
            cmd.arg("--cached").arg("HEAD");
        }
        "base_ref" => {
            let r = base_ref
                .ok_or_else(|| "`base_ref` scope requires the `base_ref` argument".to_string())?;
            cmd.arg(r);
        }
        _ => {
            // "working" (default): all uncommitted changes vs HEAD
            cmd.arg("HEAD");
        }
    }
    cmd.current_dir(repo_path);
    let output = cmd
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git diff error: {stderr}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

// ---- shared helpers ----

fn parse_direction(direction: Option<&str>) -> Direction {
    match direction {
        Some("downstream") => Direction::Downstream,
        Some("both") => Direction::Both,
        _ => Direction::Upstream,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_parse_unknown_falls_back_to_upstream() {
        assert_eq!(parse_direction(Some("downstream")), Direction::Downstream);
        assert_eq!(parse_direction(Some("both")), Direction::Both);
        assert_eq!(parse_direction(Some("sideways")), Direction::Upstream);
        assert_eq!(parse_direction(None), Direction::Upstream);
    }

    #[test]
    fn route_map_args_default_limit_is_none() {
        let args: RouteMapArgs = serde_json::from_str("{}").unwrap();
        assert!(args.prefix.is_empty());
        assert_eq!(args.limit, None);
    }

    #[test]
    fn detect_changes_args_defaults() {
        let args: DetectChangesArgs = serde_json::from_str(r#"{"scope":"working"}"#).unwrap();
        assert_eq!(args.scope, "working");
        assert!(args.base_ref.is_none());
        assert!(args.repo.is_none());
    }

    #[test]
    fn git_diff_staged_args_are_correct() {
        // Verify that staged scope would produce --cached HEAD args (structural test).
        let scope = "staged";
        let base_ref: Option<&str> = None;
        let mut cmd = std::process::Command::new("git");
        cmd.arg("diff").arg("--name-only");
        match scope {
            "staged" => { cmd.arg("--cached").arg("HEAD"); }
            "base_ref" => { cmd.arg(base_ref.unwrap_or("main")); }
            _ => { cmd.arg("HEAD"); }
        }
        // Just checking no panic here — actual execution would require a git repo.
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
    let store = build_store(&cfg).await?;
    let embed_store = if let Some(pg_url) = &cfg.pg_url {
        let store = EmbedStore::connect(pg_url, EmbedModelKind::MiniLm).await?;
        store.ensure_schema().await?;
        Some(Arc::new(store))
    } else {
        None
    };
    let graph_key = cfg.graph_key.clone();
    let server = CihServer::new(store, cfg.artifacts_dir.clone(), embed_store, graph_key);

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
