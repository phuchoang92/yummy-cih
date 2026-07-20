//! Code search MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::search::{FeatureMapCommand, QueryCommand, SearchCodeCommand};
use crate::args::{FeatureMapArgs, QueryArgs, SearchCodeArgs};

#[tool_router(router = search_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "Hybrid search over code symbols using BM25 and optional semantic embeddings."
    )]
    async fn query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .search_queries()
            .query(QueryCommand {
                repo: args.repo,
                query: args.q,
                limit: crate::search::query_limit(args.limit),
                expand: args.expand,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let output = self
            .search_queries()
            .search_code(SearchCodeCommand {
                repo: args.repo,
                query: args.query,
                limit: (if args.limit == 0 { 10 } else { args.limit }).clamp(1, 50),
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let output = self
            .search_queries()
            .feature_map(FeatureMapCommand {
                repo: args.repo,
                query: args.query,
                limit: (if args.limit == 0 { 50 } else { args.limit }).clamp(1, 200),
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
