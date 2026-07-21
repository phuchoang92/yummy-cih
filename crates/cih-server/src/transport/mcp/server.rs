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
        ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    ErrorData as McpError, RoleServer, ServerHandler,
};

use crate::application::app_services::AppServices;
use crate::domain::observability::{RequestCompletion, RequestErrorKind, RequestTransport};
use crate::domain::repository::RepoCatalogSnapshot;
use crate::infrastructure::tracing_observability::TracingObservability;
use crate::ports::observability::ObservabilityPort;

#[derive(Clone)]
pub(crate) struct CihServer {
    services: Arc<AppServices>,
    tool_router: ToolRouter<CihServer>,
    observability: Arc<dyn ObservabilityPort>,
}

impl CihServer {
    #[allow(dead_code)] // Used by transport dispatch tests; production injects observability.
    pub(crate) fn new(services: Arc<AppServices>) -> Self {
        Self {
            services,
            tool_router: crate::transport::mcp::router(),
            observability: Arc::new(TracingObservability),
        }
    }

    pub(crate) fn with_observability(
        services: Arc<AppServices>,
        observability: Arc<dyn ObservabilityPort>,
    ) -> Self {
        Self {
            services,
            tool_router: crate::transport::mcp::router(),
            observability,
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
        let capability = request.name.to_string();
        let repository_id = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("repo"))
            .and_then(|value| value.as_str())
            .map(stable_repository_id);
        let request_id = format!("{:?}", context.id);
        let started = std::time::Instant::now();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let (result, queue_wait_ms) =
            crate::ports::blocking_runtime::track_queue_wait(self.tool_router.call(tcc)).await;
        let response_bytes = result
            .as_ref()
            .ok()
            .and_then(|value| serde_json::to_vec(value).ok())
            .map(|value| value.len());
        let (result_count, completeness) = result
            .as_ref()
            .ok()
            .and_then(|result| result.structured_content.as_ref())
            .map(result_metadata)
            .unwrap_or((None, None));
        self.observability
            .record_request_completion(RequestCompletion {
                request_id,
                transport: RequestTransport::Mcp,
                capability,
                repository_id,
                duration_ms: started.elapsed().as_millis() as u64,
                queue_wait_ms: Some(queue_wait_ms),
                result_count,
                response_bytes,
                completeness,
                error_kind: result.as_ref().err().map(classify_mcp_error),
            });
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
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let started = std::time::Instant::now();
        let result = super::resources::list_resources(&self.catalog_snapshot(), request);
        self.record_protocol_completion(
            "resources/list",
            format!("{:?}", context.id),
            started,
            &result,
            result.as_ref().ok().map(|value| value.resources.len()),
            0,
        );
        result
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParam>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let started = std::time::Instant::now();
        let result = super::resources::list_resource_templates(request);
        self.record_protocol_completion(
            "resources/templates/list",
            format!("{:?}", context.id),
            started,
            &result,
            result
                .as_ref()
                .ok()
                .map(|value| value.resource_templates.len()),
            0,
        );
        result
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let started = std::time::Instant::now();
        let uri = request.uri.clone();
        let (result, queue_wait_ms) = crate::ports::blocking_runtime::track_queue_wait(async {
            if let Some((repo, slug)) = wiki_resource_target(&uri) {
                match crate::application::wiki_search::WikiPageCommand::try_new(repo, slug) {
                    Ok(command) => self
                        .wiki_page_service()
                        .get(command)
                        .await
                        .map(|content| ReadResourceResult {
                            contents: vec![ResourceContents::text(content, uri.clone())],
                        })
                        .map_err(super::error::app_error_to_mcp),
                    Err(error) => Err(super::error::app_error_to_mcp(error)),
                }
            } else {
                // Non-wiki resource reads scan artifact files — heavy lane, not the worker.
                let catalog = self.catalog_snapshot();
                match crate::ports::blocking_runtime::run_blocking_heavy(
                    crate::ports::blocking_runtime::blocking_timeout(),
                    "resource read",
                    move || super::resources::read_resource(&catalog, request),
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => Err(McpError::internal_error(error.to_string(), None)),
                }
            }
        })
        .await;
        self.record_protocol_completion(
            "resources/read",
            format!("{:?}", context.id),
            started,
            &result,
            result.as_ref().ok().map(|value| value.contents.len()),
            queue_wait_ms,
        );
        result
    }
}

fn wiki_resource_target(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("cih://repo/")?;
    if rest.contains('?') {
        return None;
    }
    super::resources::split_wiki_uri(rest).map(|(repo, slug)| (repo.to_string(), slug.to_string()))
}

fn classify_mcp_error(error: &McpError) -> RequestErrorKind {
    let message = error.message.to_ascii_lowercase();
    if message.contains("saturated") || message.contains("queue full") {
        RequestErrorKind::Overload
    } else if message.contains("timed out") || message.contains("timeout") {
        RequestErrorKind::Timeout
    } else if error.code == rmcp::model::ErrorCode::INVALID_PARAMS {
        RequestErrorKind::Protocol
    } else if message.contains("unavailable") || message.contains("retry") {
        RequestErrorKind::Dependency
    } else {
        RequestErrorKind::Internal
    }
}

impl CihServer {
    fn record_protocol_completion<T: serde::Serialize>(
        &self,
        capability: &str,
        request_id: String,
        started: std::time::Instant,
        result: &Result<T, McpError>,
        result_count: Option<usize>,
        queue_wait_ms: u64,
    ) {
        self.observability
            .record_request_completion(RequestCompletion {
                request_id,
                transport: RequestTransport::Mcp,
                capability: capability.to_string(),
                repository_id: None,
                duration_ms: started.elapsed().as_millis() as u64,
                queue_wait_ms: Some(queue_wait_ms),
                result_count,
                response_bytes: result
                    .as_ref()
                    .ok()
                    .and_then(|value| serde_json::to_vec(value).ok())
                    .map(|bytes| bytes.len()),
                completeness: None,
                error_kind: result.as_ref().err().map(classify_mcp_error),
            });
    }
}

fn stable_repository_id(repository: &str) -> String {
    // FNV-1a gives a deterministic bounded identifier without exposing an
    // arbitrary repository name as a metrics label.
    let hash = repository
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("repo-{hash:016x}")
}

fn result_metadata(value: &serde_json::Value) -> (Option<usize>, Option<String>) {
    let result_count = value.as_array().map(Vec::len).or_else(|| {
        value.as_object().and_then(|object| {
            ["count", "step_count", "total"]
                .into_iter()
                .find_map(|key| object.get(key).and_then(serde_json::Value::as_u64))
                .map(|count| count as usize)
                .or_else(|| {
                    ["items", "results", "communities", "routes", "steps"]
                        .into_iter()
                        .find_map(|key| object.get(key).and_then(serde_json::Value::as_array))
                        .map(Vec::len)
                })
        })
    });
    let completeness = value
        .get("completeness")
        .and_then(serde_json::Value::as_object)
        .and_then(|metadata| metadata.get("complete"))
        .and_then(serde_json::Value::as_bool)
        .map(|complete| if complete { "complete" } else { "partial" }.to_string());
    (result_count, completeness)
}
#[cfg(test)]
mod tests {
    use super::{classify_mcp_error, result_metadata, stable_repository_id};

    #[test]
    fn completion_metadata_is_bounded_and_detects_partial_results() {
        assert_eq!(stable_repository_id("repo-a").len(), 21);
        assert_eq!(
            stable_repository_id("repo-a"),
            stable_repository_id("repo-a")
        );
        assert_ne!(
            stable_repository_id("repo-a"),
            stable_repository_id("repo-b")
        );
        let value = serde_json::json!({
            "items": [1, 2],
            "completeness": { "complete": false }
        });
        assert_eq!(result_metadata(&value), (Some(2), Some("partial".into())));
    }

    #[test]
    fn completion_errors_use_bounded_classes() {
        assert_eq!(
            classify_mcp_error(&rmcp::ErrorData::invalid_params("bad", None)),
            crate::domain::observability::RequestErrorKind::Protocol
        );
        assert_eq!(
            classify_mcp_error(&rmcp::ErrorData::internal_error("queue full", None)),
            crate::domain::observability::RequestErrorKind::Overload
        );
        assert_eq!(
            classify_mcp_error(&rmcp::ErrorData::internal_error("load timed out", None)),
            crate::domain::observability::RequestErrorKind::Timeout
        );
    }

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
