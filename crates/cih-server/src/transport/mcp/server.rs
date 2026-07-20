//! CIH MCP server transport, tool routing, resources, and protocol metadata.
//!
//! Exposes code-intelligence application services as MCP tools. Storage,
//! repository, search, and artifact adapters are assembled in `bootstrap` and
//! reached only through the injected `AppServices`.
//!
//! ⚠️ rmcp version note: the `#[tool_router]` / `#[tool]` / `ServerHandler`
//! macros and the `StreamableHttpService` constructor shape move between rmcp
//! releases. If `cargo build` flags the wiring below, reconcile it against
//! docs.rs for the version you resolve. Protocol-specific changes stay in this
//! transport namespace.

use std::sync::Arc;

use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{
        CallToolResult, Implementation, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParam, ProtocolVersion, ReadResourceRequestParam,
        ReadResourceResult, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    ErrorData as McpError, RoleServer, ServerHandler,
};

use crate::application::app_services::AppServices;
use crate::domain::repository::RepoCatalogSnapshot;

#[derive(Clone)]
pub(crate) struct CihServer {
    services: Arc<AppServices>,
    tool_router: ToolRouter<CihServer>,
}

impl CihServer {
    pub(crate) fn new(services: Arc<AppServices>) -> Self {
        Self {
            services,
            tool_router: crate::transport::mcp::router(),
        }
    }

    fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
        self.services.repos.catalog_snapshot()
    }

    pub(crate) fn architecture_overview_service(
        &self,
    ) -> &crate::application::architecture_overview::ArchitectureOverviewService {
        &self.services.graph.architecture_overview
    }

    pub(crate) fn contract_service(&self) -> &crate::application::contracts::ContractService {
        &self.services.cross_repo.contracts
    }

    pub(crate) fn indexing_service(&self) -> &crate::application::indexing::IndexingService {
        &self.services.admin.indexing
    }

    pub(crate) fn resolve_pattern_service(
        &self,
    ) -> &crate::application::admin::resolve_patterns::ResolvePatternService {
        &self.services.admin.patterns
    }

    pub(crate) fn testing_service(&self) -> &crate::application::testing::TestingService {
        &self.services.testing.analysis
    }

    pub(crate) fn file_service(&self) -> &crate::application::files::FileService {
        &self.services.files.access
    }

    pub(crate) fn wiki_search_service(
        &self,
    ) -> &crate::application::wiki_search::WikiSearchService {
        &self.services.docs.wiki_search
    }

    pub(crate) fn wiki_page_service(&self) -> &crate::application::wiki_search::WikiPageService {
        &self.services.docs.wiki_page
    }

    pub(crate) fn graph_queries(&self) -> &crate::application::graph::GraphQueryService {
        &self.services.graph.queries
    }

    pub(crate) fn search_queries(&self) -> &crate::application::search::SearchService {
        &self.services.search.queries
    }

    pub(crate) fn repository_admin(&self) -> &crate::application::admin::RepositoryAdminService {
        &self.services.admin.repositories
    }
}

/// Agent-facing server instructions returned by `get_info`. Every
/// `tool(arg=...)` example in here (and in tool descriptions) is validated
/// against the live tool schemas by `instruction_examples_match_tool_schemas`,
/// so an argument rename fails the build instead of teaching clients a
/// hallucinated call.
const INSTRUCTIONS: &str =
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
     6. `trace_flow(entry_point=\"Route:METHOD /path\")` — follow an HTTP request end-to-end; add format=\"mermaid\" for a diagram\n\
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
     `taint_paths(category=\"sql\"|\"exec\"|\"file\"|\"html\")` — source→sink flows from HTTP/event entry points; refine=true for flow-sensitive confirmation.";

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
            instructions: Some(INSTRUCTIONS.into()),
        }
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        super::resources::list_resources(&self.catalog_snapshot(), request)
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        super::resources::list_resource_templates(request)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        // Resource reads scan artifact files — heavy lane, not the worker.
        let catalog = self.catalog_snapshot();
        crate::ports::blocking_runtime::run_blocking_heavy(
            crate::ports::blocking_runtime::blocking_timeout(),
            "resource read",
            move || super::resources::read_resource(&catalog, request),
        )
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn split_routers_register_all_tools_without_dropping_any() {
        // Guards the transport split: the per-theme `#[tool_router]` impls must
        // merge into the same tool surface, with every moved tool still present.
        let router = crate::transport::mcp::router();
        assert_eq!(
            router.list_all().len(),
            31,
            "tool count changed after the split — a tool was dropped or duplicated"
        );
        for tool in [
            "read_file",
            "grep_files",
            "group_contracts",
            "taint_paths",
            "search_wiki",
            "index_repo",
            "index_cancel",
            "impact",
            "architecture_overview",
        ] {
            assert!(router.has_route(tool), "missing tool after split: {tool}");
        }
        // Every tool an architecture_overview `next` hint can emit must be a
        // real route — a hint that drifts from the tool surface teaches clients
        // hallucinated calls.
        for tool in crate::application::architecture_overview::HINT_TOOLS {
            assert!(
                router.has_route(tool),
                "overview next-hint references unregistered tool: {tool}"
            );
        }
    }

    /// Every `tool(arg=...)` example in the server instructions and in tool
    /// descriptions must name a real tool argument — this is what catches
    /// drift like the former `trace_flow(name=...)` (real arg: `entry_point`).
    #[test]
    fn instruction_examples_match_tool_schemas() {
        let router = crate::transport::mcp::router();
        let tools = router.list_all();
        let schemas: std::collections::HashMap<String, serde_json::Value> = tools
            .iter()
            .map(|t| {
                (
                    t.name.to_string(),
                    serde_json::to_value(t.input_schema.as_ref()).expect("schema serializes"),
                )
            })
            .collect();
        let mut texts = vec![super::INSTRUCTIONS.to_string()];
        texts.extend(
            tools
                .iter()
                .filter_map(|t| t.description.as_ref().map(|d| d.to_string())),
        );

        let call_re = regex::Regex::new(r"\b([a-z_][a-z0-9_]*)\(([^()]*)\)").unwrap();
        let mut checked = 0usize;
        for text in &texts {
            for cap in call_re.captures_iter(text) {
                let Some(schema) = schemas.get(&cap[1]) else {
                    continue; // not a tool call (prose that happens to parenthesize)
                };
                let props = schema.get("properties").and_then(|p| p.as_object());
                for part in cap[2].split(',') {
                    if !part.contains('=') {
                        continue;
                    }
                    let arg = part.split('=').next().unwrap_or("").trim();
                    if arg.is_empty()
                        || !arg
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    {
                        continue;
                    }
                    checked += 1;
                    assert!(
                        props.is_some_and(|p| p.contains_key(arg)),
                        "example `{}({arg}=...)` names an argument that is not in the tool's \
                         schema — fix the instructions/description or the args struct",
                        &cap[1]
                    );
                }
            }
        }
        // The regex must actually be finding examples, or this test is vacuous.
        assert!(checked >= 10, "only {checked} examples validated");
        // Regression pin for the drift this test was added to catch.
        assert!(super::INSTRUCTIONS.contains("trace_flow(entry_point="));
    }
}
