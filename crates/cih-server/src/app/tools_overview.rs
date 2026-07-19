//! `architecture_overview` MCP tool — one-call orientation for an indexed repo.
//! Composition lives in `crate::overview` (free functions over `&dyn GraphStore`
//! plus artifact paths, for hermetic testing); this router is the thin MCP shim.
//! Design record: `docs/plans/architecture-overview-tool.md`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::CihServer;
use crate::args::ArchitectureOverviewArgs;
use crate::overview;
use crate::utils::json_result;

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
        let entry = crate::utils::resolve_repo_entry(&args.repo, &self.graph_key)
            .map_err(|e| McpError::invalid_params(e, None))?;
        let store = self.store_for(&entry.graph_key).await?;
        let reg = cih_core::Registry::load_cached();
        let registry_stale = reg.is_stale(&entry.name);
        let groups = overview::group_sections(&entry.name, &reg);
        let wiki = crate::wiki::list_pages(&self.wiki, &args.repo).await?;
        let response = overview::compose(overview::ComposeCtx {
            store: store.as_ref(),
            entry: &entry,
            registry_stale,
            groups,
            wiki,
            sections: args.sections,
            limit: args.limit,
        })
        .await?;
        json_result(&response)
    }
}
