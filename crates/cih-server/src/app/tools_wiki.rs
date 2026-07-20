//! Wiki MCP tools (`search_wiki`, `get_wiki_page`), split out of the `app.rs`
//! god-module. Merged via `+ Self::wiki_router()` in `CihServer::new`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::CihServer;
use crate::args::{GetWikiPageArgs, SearchWikiArgs};
use crate::wiki;

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
        let repo = self.resolve_repo(&args.repo)?;
        wiki::search_wiki(&self.wiki, &repo, args).await
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
        let repo = self.resolve_repo(&args.repo)?;
        wiki::get_wiki_page(&self.wiki, &repo, args).await
    }
}
