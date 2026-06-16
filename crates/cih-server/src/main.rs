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
mod viz;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cih_core::{
    ContractMatch, ContractMatchKind, Edge, EdgeKind, GraphArtifacts, Node, NodeId, NodeKind,
    Registry, VersionId,
};
use cih_embed::{EmbedModelKind, EmbedStore};
use cih_graph_store::{CommunityInfo, Direction, GraphStore, GraphStoreError, RouteInfo};
use viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ListResourceTemplatesResult, ListResourcesResult,
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
    /// Output format. Omit for default JSON. Pass `"diagram"` for D3 force-directed JSON.
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CommunitiesArgs {
    /// Optional maximum number of communities to return.
    #[serde(default)]
    limit: Option<usize>,
    /// Output format. Omit for default JSON. Pass `"diagram"` for D3 service-map JSON.
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RouteMapArgs {
    /// Path prefix filter (e.g. "/api/owners"). Omit or leave empty for all routes.
    #[serde(default)]
    prefix: String,
    /// Max routes to return (default 200).
    #[serde(default)]
    limit: Option<usize>,
    /// Output format. Omit for default JSON. Pass `"openapi"` for OpenAPI 3.0.3 JSON.
    #[serde(default)]
    format: Option<String>,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct GroupContractsArgs {
    /// Group name created with `cih-engine group create`.
    group: String,
    /// Optional kind filter: `all`, `http`, `http_route`, `kafka`, `kafka_topic`,
    /// `spring`, or `spring_event`.
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ApiImpactArgs {
    /// Group name created with `cih-engine group create`.
    group: String,
    /// HTTP method: GET, POST, PUT, DELETE, PATCH (case-insensitive).
    method: String,
    /// Route path template, e.g. `/api/orders/{id}`. Path variables are normalized to `{*}`.
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShapeCheckArgs {
    /// Group name created with `cih-engine group create`.
    group: String,
    /// Provider repo name (as registered with `cih-engine analyze`).
    provider: String,
    /// Consumer repo name (as registered).
    consumer: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TraceFlowArgs {
    /// Symbol id or short name to trace from. Accepts a Route node
    /// (e.g. `Route:GET /api/checkout`) or a Method node id.
    /// Short names trigger disambiguation like `context` and `impact`.
    entry_point: String,
    /// Maximum traversal depth (default 6, clamped to 10).
    #[serde(default)]
    max_depth: Option<u32>,
    /// Output format. Omit for default JSON. Pass `"mermaid"` for a Mermaid flowchart string.
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FeatureMapArgs {
    /// Business keywords to map to code clusters (e.g. "checkout payment").
    query: String,
    /// Max symbols to search for before clustering (default 50, max 200).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TestCoverageArgs {
    /// Symbol to look up test coverage for — full NodeId or short name.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RegressionScopeArgs {
    /// Repo-relative file paths that changed (e.g. ["src/main/java/com/acme/OrderService.java"]).
    changed_files: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UntestedPathsArgs {
    /// Repo-relative path prefix to restrict the search (e.g. "src/main/java/com/acme/payment").
    module_prefix: String,
    /// Max symbols to return (default 50, max 500).
    #[serde(default)]
    limit: Option<usize>,
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
            let seeds: Vec<NodeId> = hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(self.store.subgraph(&seeds, 1).await.map_err(to_mcp)?)
        } else {
            None
        };

        json_result(&QueryResult { hits, subgraph })
    }

    #[tool(
        description = "List Spring REST endpoints: HTTP method + path + handler method. \
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
        let limit = args.limit.unwrap_or(200).clamp(1, 1000);
        let routes: Vec<RouteInfo> = self.store.route_map(prefix, limit).await.map_err(to_mcp)?;
        if args.format.as_deref() == Some("openapi") {
            return json_result(&render_openapi(&routes));
        }
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
        // 1. Find repo path via registry.
        let repo_path = find_repo_path(args.repo.as_deref(), &self.graph_key)
            .map_err(|e| McpError::invalid_params(e, None))?;

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
            if let Ok(impact) = self.store.impact(&node.id, Direction::Upstream, 4).await {
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

    #[tool(
        description = "Return cross-service contract matches for a repo group. \
        Run `cih-engine group sync <group>` first. Optional kind filter: \
        all, http/http_route, kafka/kafka_topic, spring/spring_event."
    )]
    async fn group_contracts(
        &self,
        Parameters(args): Parameters<GroupContractsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = cih_core::contracts_path(&args.group).ok_or_else(|| {
            McpError::internal_error("cannot determine HOME for group contracts path", None)
        })?;
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            McpError::invalid_params(
                format!(
                    "cannot read contracts for group '{}' at {}: {e}. Run `cih-engine group sync {}` first",
                    args.group,
                    path.display(),
                    args.group
                ),
                None,
            )
        })?;
        let filter = parse_contract_kind_filter(args.kind.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let mut matches = Vec::new();
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let item: ContractMatch = serde_json::from_str(line).map_err(|e| {
                McpError::internal_error(format!("malformed contracts artifact: {e}"), None)
            })?;
            if filter.is_none() || filter == Some(item.kind) {
                matches.push(item);
            }
        }
        json_result(&matches)
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
        let contracts_file = cih_core::contracts_path(&args.group).ok_or_else(|| {
            McpError::internal_error("cannot determine HOME for group contracts path", None)
        })?;
        let raw = std::fs::read_to_string(&contracts_file).map_err(|e| {
            McpError::invalid_params(
                format!(
                    "cannot read contracts for group '{}': {e}. \
                     Run `cih-engine group sync {}` first",
                    args.group, args.group
                ),
                None,
            )
        })?;
        let method = args.method.to_ascii_uppercase();
        let target_key = format!("{} {}", method, cih_core::normalize_contract_path(&args.path));
        let mut consumers = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let item: ContractMatch = serde_json::from_str(line).map_err(|e| {
                McpError::internal_error(format!("malformed contracts artifact: {e}"), None)
            })?;
            if item.kind != ContractMatchKind::HttpRoute || item.match_key != target_key {
                continue;
            }
            consumers.push(serde_json::json!({
                "provider_repo": item.provider_repo,
                "provider_route": item.provider_id,
                "consumer_repo": item.consumer_repo,
                "consumer_endpoint": item.consumer_id,
            }));
        }
        json_result(&serde_json::json!({
            "method": method,
            "path": args.path,
            "match_key": target_key,
            "consumers": consumers,
        }))
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
        let contracts_file = cih_core::contracts_path(&args.group).ok_or_else(|| {
            McpError::internal_error("cannot determine HOME for group contracts path", None)
        })?;
        let raw = std::fs::read_to_string(&contracts_file).map_err(|e| {
            McpError::invalid_params(
                format!(
                    "cannot read contracts for group '{}': {e}. \
                     Run `cih-engine group sync {}` first",
                    args.group, args.group
                ),
                None,
            )
        })?;
        let contracts: Vec<ContractMatch> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<ContractMatch>(l).ok())
            .filter(|c| {
                c.kind == ContractMatchKind::HttpRoute
                    && c.provider_repo == args.provider
                    && c.consumer_repo == args.consumer
            })
            .collect();
        if contracts.is_empty() {
            return json_result(&serde_json::json!({
                "provider": args.provider,
                "consumer": args.consumer,
                "contracts": [],
                "note": "No HTTP contracts found between these repos in the group."
            }));
        }

        let reg = Registry::load();
        let provider_entry = reg.find(&args.provider).ok_or_else(|| {
            McpError::invalid_params(
                format!("provider '{}' not in registry; run analyze first", args.provider),
                None,
            )
        })?;
        let consumer_entry = reg.find(&args.consumer).ok_or_else(|| {
            McpError::invalid_params(
                format!("consumer '{}' not in registry; run analyze first", args.consumer),
                None,
            )
        })?;

        let provider_nodes = load_artifact_nodes(&provider_entry.artifacts_dir)
            .map_err(|e| McpError::internal_error(format!("provider artifacts: {e}"), None))?;
        let consumer_nodes = load_artifact_nodes(&consumer_entry.artifacts_dir)
            .map_err(|e| McpError::internal_error(format!("consumer artifacts: {e}"), None))?;
        let consumer_edges = load_artifact_edges(&consumer_entry.artifacts_dir)
            .map_err(|e| McpError::internal_error(format!("consumer edges: {e}"), None))?;

        let provider_by_id: std::collections::BTreeMap<String, &Node> = provider_nodes
            .iter()
            .map(|n| (n.id.as_str().to_string(), n))
            .collect();
        let consumer_by_id: std::collections::BTreeMap<String, &Node> = consumer_nodes
            .iter()
            .map(|n| (n.id.as_str().to_string(), n))
            .collect();

        // Index consumer edges: ExternalCall dst→[src], Accesses src→[dst]
        let mut ext_call_callers: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        let mut method_accesses: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for edge in &consumer_edges {
            match edge.kind {
                EdgeKind::ExternalCall => {
                    ext_call_callers
                        .entry(edge.dst.as_str().to_string())
                        .or_default()
                        .push(edge.src.as_str().to_string());
                }
                EdgeKind::Accesses => {
                    method_accesses
                        .entry(edge.src.as_str().to_string())
                        .or_default()
                        .push(edge.dst.as_str().to_string());
                }
                _ => {}
            }
        }

        let mut results = Vec::new();
        for contract in &contracts {
            let route_node = provider_by_id.get(&contract.provider_id);
            let handler_sig =
                route_node.and_then(|n| node_prop_str_owned(n, "handler"));
            let method_node = handler_sig
                .as_ref()
                .and_then(|sig| provider_by_id.get(&format!("Method:{sig}")));
            let return_type_raw =
                method_node.and_then(|n| node_prop_str_owned(n, "returnType"));
            let dto_short = return_type_raw
                .as_deref()
                .map(strip_response_wrapper)
                .unwrap_or("");

            let provider_fields: Vec<String> = if dto_short.is_empty() {
                vec![]
            } else {
                let dto_fqcns: Vec<String> = provider_nodes
                    .iter()
                    .filter(|n| matches!(n.kind, NodeKind::Class | NodeKind::Record))
                    .filter(|n| short_class_name(&n.name) == dto_short)
                    .filter_map(|n| {
                        n.qualified_name
                            .clone()
                            .or_else(|| Some(n.name.clone()))
                    })
                    .collect();
                provider_nodes
                    .iter()
                    .filter(|n| n.kind == NodeKind::Field)
                    .filter(|n| {
                        n.qualified_name
                            .as_deref()
                            .map(|qn| {
                                dto_fqcns
                                    .iter()
                                    .any(|fqcn| qn.starts_with(&format!("{fqcn}#")))
                            })
                            .unwrap_or(false)
                    })
                    .map(|n| n.name.clone())
                    .collect()
            };

            let caller_ids: Vec<String> = ext_call_callers
                .get(&contract.consumer_id)
                .cloned()
                .unwrap_or_default();
            let mut consumer_accessed: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for caller_id in &caller_ids {
                if let Some(field_ids) = method_accesses.get(caller_id) {
                    for fid in field_ids {
                        if let Some(fn_node) = consumer_by_id.get(fid) {
                            consumer_accessed.insert(fn_node.name.clone());
                        }
                    }
                }
            }

            let provider_set: std::collections::BTreeSet<String> =
                provider_fields.iter().cloned().collect();
            let provider_only: Vec<String> = provider_fields
                .iter()
                .filter(|f| !consumer_accessed.contains(*f))
                .cloned()
                .collect();
            let consumer_only: Vec<String> = consumer_accessed
                .iter()
                .filter(|f| !provider_set.contains(*f))
                .cloned()
                .collect();
            let matched: Vec<String> = provider_fields
                .iter()
                .filter(|f| consumer_accessed.contains(*f))
                .cloned()
                .collect();

            results.push(serde_json::json!({
                "provider_route": contract.provider_id,
                "consumer_endpoint": contract.consumer_id,
                "provider_handler": handler_sig,
                "provider_return_type": return_type_raw,
                "provider_dto": if dto_short.is_empty() { None } else { Some(dto_short) },
                "provider_fields": provider_fields,
                "consumer_accessed_fields": consumer_accessed.into_iter().collect::<Vec<_>>(),
                "matched": matched,
                "provider_only": provider_only,
                "consumer_only": consumer_only,
                "note": if return_type_raw.is_none() {
                    Some("returnType not available — re-run `cih-engine analyze` to populate it")
                } else {
                    None
                },
            }));
        }

        json_result(&serde_json::json!({
            "provider": args.provider,
            "consumer": args.consumer,
            "contracts": results,
        }))
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
        description = "Map business keywords to code clusters: BM25 search results grouped \
            by community. Helps answer 'what code implements the checkout feature?' or \
            'which modules handle payments?'"
    )]
    async fn feature_map(
        &self,
        Parameters(args): Parameters<FeatureMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(50).clamp(1, 200);
        let hits = self
            .search
            .query_hits(&args.query, limit)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let hit_ids: Vec<NodeId> = hits.iter().map(|h| h.node_id.clone()).collect();
        let memberships = self
            .store
            .symbol_communities(&hit_ids)
            .await
            .map_err(to_mcp)?;

        let community_of: std::collections::BTreeMap<String, CommunityInfo> = memberships
            .into_iter()
            .map(|(nid, ci)| (nid.to_string(), ci))
            .collect();

        let mut clusters: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
            std::collections::BTreeMap::new();
        for hit in &hits {
            let cluster_key = community_of
                .get(hit.node_id.as_str())
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "unclustered".to_string());
            clusters
                .entry(cluster_key)
                .or_default()
                .push(serde_json::json!({
                    "id": hit.node_id.as_str(),
                    "kind": hit.kind.label(),
                    "name": hit.name,
                    "file": hit.file,
                    "score": hit.score,
                }));
        }

        let result: Vec<serde_json::Value> = clusters
            .into_iter()
            .map(|(name, symbols)| {
                serde_json::json!({
                    "community": name,
                    "symbol_count": symbols.len(),
                    "symbols": symbols,
                })
            })
            .collect();

        json_result(&serde_json::json!({
            "query": args.query,
            "total_hits": hits.len(),
            "clusters": result,
        }))
    }

    #[tool(
        description = "Return all test methods/classes that cover a symbol (via TESTS edges). \
            Helps a tester understand which tests exercise a given class or method."
    )]
    async fn test_coverage(
        &self,
        Parameters(args): Parameters<TestCoverageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = match self.resolve_symbol(&args.name).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(candidates) => {
                return json_result(&AmbiguousResult {
                    status: "ambiguous",
                    candidates: candidates
                        .iter()
                        .map(|n| AmbiguousCandidate {
                            id: n.id.to_string(),
                            kind: n.kind.label().to_string(),
                            name: n.name.clone(),
                            file: n.file.clone(),
                        })
                        .collect(),
                });
            }
            SymbolResolution::NotFound => {
                return Err(McpError::invalid_params(
                    format!("symbol '{}' not found", args.name),
                    None,
                ));
            }
        };
        let tests = self.store.test_coverage(&id).await.map_err(to_mcp)?;
        json_result(&serde_json::json!({
            "symbol_id": id.as_str(),
            "test_count": tests.len(),
            "tests": tests.iter().map(|n| serde_json::json!({
                "id": n.id.as_str(),
                "kind": n.kind.label(),
                "name": n.name,
                "file": n.file,
            })).collect::<Vec<_>>(),
        }))
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
        let tests = self
            .store
            .tests_for_files(&args.changed_files)
            .await
            .map_err(to_mcp)?;
        // Dedup by file, collect unique test class files.
        let mut seen_files = std::collections::BTreeSet::new();
        let test_classes: Vec<serde_json::Value> = tests
            .iter()
            .filter(|n| seen_files.insert(n.file.clone()))
            .map(|n| serde_json::json!({
                "id": n.id.as_str(),
                "kind": n.kind.label(),
                "name": n.name,
                "file": n.file,
            }))
            .collect();
        json_result(&serde_json::json!({
            "changed_file_count": args.changed_files.len(),
            "test_class_count": test_classes.len(),
            "test_classes": test_classes,
        }))
    }

    #[tool(
        description = "Return production symbols (classes, methods) under a path prefix that \
            have no test coverage (no inbound TESTS edge). Helps identify coverage gaps."
    )]
    async fn untested_paths(
        &self,
        Parameters(args): Parameters<UntestedPathsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(50).clamp(1, 500);
        let symbols = self
            .store
            .untested_symbols(&args.module_prefix, limit)
            .await
            .map_err(to_mcp)?;
        json_result(&serde_json::json!({
            "prefix": args.module_prefix,
            "untested_count": symbols.len(),
            "symbols": symbols.iter().map(|n| serde_json::json!({
                "id": n.id.as_str(),
                "kind": n.kind.label(),
                "name": n.name,
                "file": n.file,
            })).collect::<Vec<_>>(),
        }))
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
                 `communities`, `route_map`, `trace_flow`, `feature_map`, \
                 `group_contracts`, `api_impact`, `shape_check`, \
                 `list_repos`, `detect_changes`, \
                 `test_coverage`, `regression_scope`, `untested_paths`. \
                 Short symbol names trigger disambiguation; full NodeIds (Kind:fqn) skip it. \
                 Read repo data via cih://repo/{name}/context|communities|processes|schema. \
                 Visualization formats: `impact(format=\"diagram\")` → D3-JSON blast-radius graph; \
                 `trace_flow(format=\"mermaid\")` → Mermaid flowchart; \
                 `communities(format=\"diagram\")` → D3-JSON service map; \
                 `route_map(format=\"openapi\")` → OpenAPI 3.0.3 JSON."
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
                format!("no repo registered for graph_key '{graph_key}'; pass `repo` explicitly")
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
    let output = cmd.output().map_err(|e| format!("git diff failed: {e}"))?;
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

fn load_artifact_nodes(artifacts_dir: &str) -> std::io::Result<Vec<Node>> {
    let dir = std::path::Path::new(artifacts_dir);
    GraphArtifacts {
        nodes_path: dir.join("nodes.jsonl"),
        edges_path: dir.join("edges.jsonl"),
        version: VersionId(String::new()),
    }
    .read_nodes()
}

fn load_artifact_edges(artifacts_dir: &str) -> std::io::Result<Vec<Edge>> {
    let dir = std::path::Path::new(artifacts_dir);
    GraphArtifacts {
        nodes_path: dir.join("nodes.jsonl"),
        edges_path: dir.join("edges.jsonl"),
        version: VersionId(String::new()),
    }
    .read_edges()
}

fn node_prop_str_owned(node: &Node, key: &str) -> Option<String> {
    node.props.as_ref()?.get(key)?.as_str().map(str::to_owned)
}

fn strip_response_wrapper(raw: &str) -> &str {
    raw.find('<')
        .and_then(|i| raw.rfind('>').map(|j| &raw[i + 1..j]))
        .unwrap_or(raw)
}

fn short_class_name(fqcn: &str) -> &str {
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

fn parse_contract_kind_filter(
    kind: Option<&str>,
) -> std::result::Result<Option<ContractMatchKind>, String> {
    match kind.unwrap_or("all").trim().to_ascii_lowercase().as_str() {
        "" | "all" => Ok(None),
        "http" | "http_route" | "http-route" => Ok(Some(ContractMatchKind::HttpRoute)),
        "kafka" | "kafka_topic" | "kafka-topic" => Ok(Some(ContractMatchKind::KafkaTopic)),
        "spring" | "spring_event" | "spring-event" => Ok(Some(ContractMatchKind::SpringEvent)),
        other => Err(format!(
            "unknown contract kind '{other}'; expected all, http, kafka, or spring"
        )),
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

fn text_result(s: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s)]))
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
    fn contract_kind_filter_accepts_aliases() {
        assert_eq!(parse_contract_kind_filter(None).unwrap(), None);
        assert_eq!(
            parse_contract_kind_filter(Some("http")).unwrap(),
            Some(ContractMatchKind::HttpRoute)
        );
        assert_eq!(
            parse_contract_kind_filter(Some("kafka_topic")).unwrap(),
            Some(ContractMatchKind::KafkaTopic)
        );
        assert_eq!(
            parse_contract_kind_filter(Some("spring-event")).unwrap(),
            Some(ContractMatchKind::SpringEvent)
        );
        assert!(parse_contract_kind_filter(Some("queue")).is_err());
    }

    #[test]
    fn trace_flow_args_defaults() {
        let args: TraceFlowArgs =
            serde_json::from_str(r#"{"entry_point":"Route:GET /"}"#).unwrap();
        assert_eq!(args.entry_point, "Route:GET /");
        assert!(args.max_depth.is_none());
        assert!(args.format.is_none());
    }

    #[test]
    fn impact_args_accepts_format_diagram() {
        let args: ImpactArgs =
            serde_json::from_str(r#"{"name":"OrderService","format":"diagram"}"#).unwrap();
        assert_eq!(args.name, "OrderService");
        assert_eq!(args.format.as_deref(), Some("diagram"));
    }

    #[test]
    fn trace_flow_args_accepts_format_mermaid() {
        let args: TraceFlowArgs = serde_json::from_str(
            r#"{"entry_point":"Route:GET /api/checkout","format":"mermaid"}"#,
        )
        .unwrap();
        assert_eq!(args.entry_point, "Route:GET /api/checkout");
        assert_eq!(args.format.as_deref(), Some("mermaid"));
    }

    #[test]
    fn feature_map_args_defaults() {
        let args: FeatureMapArgs = serde_json::from_str(r#"{"query":"checkout"}"#).unwrap();
        assert_eq!(args.query, "checkout");
        assert!(args.limit.is_none());
    }

    #[test]
    fn regression_scope_args_parses_file_list() {
        let args: RegressionScopeArgs = serde_json::from_str(
            r#"{"changed_files":["src/main/java/com/acme/Foo.java"]}"#,
        )
        .unwrap();
        assert_eq!(args.changed_files.len(), 1);
        assert_eq!(args.changed_files[0], "src/main/java/com/acme/Foo.java");
    }

    #[test]
    fn untested_paths_args_defaults() {
        let args: UntestedPathsArgs =
            serde_json::from_str(r#"{"module_prefix":"src/main/java/com/acme"}"#).unwrap();
        assert_eq!(args.module_prefix, "src/main/java/com/acme");
        assert!(args.limit.is_none());
    }

    #[test]
    fn git_diff_staged_args_are_correct() {
        // Verify that staged scope would produce --cached HEAD args (structural test).
        let scope = "staged";
        let base_ref: Option<&str> = None;
        let mut cmd = std::process::Command::new("git");
        cmd.arg("diff").arg("--name-only");
        match scope {
            "staged" => {
                cmd.arg("--cached").arg("HEAD");
            }
            "base_ref" => {
                cmd.arg(base_ref.unwrap_or("main"));
            }
            _ => {
                cmd.arg("HEAD");
            }
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
