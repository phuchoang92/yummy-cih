//! Wiki MCP adapters (`search_wiki`, `get_wiki_page`).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result, text_result};
use super::super::CihServer;
use crate::application::wiki_search::{WikiPageCommand, WikiSearchCommand};
use crate::args::{GetWikiPageArgs, SearchWikiArgs};

#[tool_router(router = wiki_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "Search the generated wiki (role-based docs produced by `cih-engine wiki`). \
            BM25 over page titles and bodies. Facets: kind (persona pages carry their persona \
            as the kind — `po`, `ba`, `dev` — plus `index`, `routes`, `api-flow`), role (the \
            feature/module grouping, e.g. `loan`, `system`), and feature (community id). \
            Returns ranked hits with slug, title, snippet, and provenance (graph_version, \
            generated_at). Use get_wiki_page(slug=...) to fetch a full page."
    )]
    async fn search_wiki(
        &self,
        Parameters(args): Parameters<SearchWikiArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command = WikiSearchCommand::try_new(
            args.query,
            args.repo,
            Some(args.role),
            Some(args.kind),
            Some(args.feature),
            (args.limit > 0).then_some(args.limit),
        )
        .map_err(app_error_to_mcp)?;
        let output = self
            .wiki_search_service()
            .search(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Fetch one generated wiki page's full markdown by slug (find slugs with \
            search_wiki). The YAML front matter carries provenance: enrichment tier and \
            graph_version."
    )]
    async fn get_wiki_page(
        &self,
        Parameters(args): Parameters<GetWikiPageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command = WikiPageCommand::try_new(args.repo, args.slug).map_err(app_error_to_mcp)?;
        let markdown = self
            .wiki_page_service()
            .get(command)
            .await
            .map_err(app_error_to_mcp)?;
        text_result(markdown)
    }
}
