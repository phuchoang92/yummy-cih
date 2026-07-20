//! `architecture_overview` MCP tool — one-call orientation for an indexed repo.
//! The typed application service owns orchestration; this router only maps the
//! wire request and translates the application result back to MCP.
//! Design record: `docs/plans/architecture-overview-tool.md`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::architecture_overview::ArchitectureOverviewCommand;
use crate::args::ArchitectureOverviewArgs;

#[tool_router(router = overview_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "One-call architectural orientation for an indexed repo. Returns compact, \
            size-capped sections: stats (per-kind node/edge counts), modules (detected module \
            clusters with anchor symbol ids), route_groups (endpoints bucketed by path prefix, \
            with trace_flow-ready sample routes), entrypoints (schedulers, listeners, high-degree \
            hubs), wiki_pages (slugs for get_wiki_page), plus provenance and warnings. Call this \
            FIRST after list_repos when orienting in an unfamiliar codebase — it replaces \
            chaining status/communities/route_map/search_wiki. Call it once per repo per \
            session; go deeper with the narrow tools it points to (context on an anchor symbol, \
            trace_flow on a sample route, route_map(prefix=...), get_wiki_page(slug=...)) rather \
            than re-calling with a larger limit. Truncated lists carry a ready-to-use `next` \
            call; a section with \"available\": false means a pipeline step has not run (its \
            `remedy` says which command) — it is NOT a fact about the codebase. Optional: \
            sections=[...] to select sections (\"hotspots\" is opt-in), limit to scale list \
            sizes, repo to target a non-primary repo."
    )]
    async fn architecture_overview(
        &self,
        Parameters(args): Parameters<ArchitectureOverviewArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command = ArchitectureOverviewCommand::try_new(args.repo, args.sections, args.limit)
            .map_err(app_error_to_mcp)?;
        let response = self
            .architecture_overview_service()
            .execute(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&response)
    }
}
