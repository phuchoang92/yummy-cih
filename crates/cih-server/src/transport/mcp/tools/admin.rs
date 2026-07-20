//! Repository catalog MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::admin::RepoStatusCommand;
use crate::transport::mcp::args::{ListReposArgs, StatusArgs};

#[tool_router(router = repository_admin_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(description = "List all repos indexed in the CIH registry with their stats.")]
    async fn list_repos(&self, _: Parameters<ListReposArgs>) -> Result<CallToolResult, McpError> {
        json_result(&self.repository_admin().list_repos())
    }

    #[tool(
        description = "Return registry entry and staleness for one repo (by name or path), \
        plus contract-sync freshness for every group the repo belongs to."
    )]
    async fn status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .repository_admin()
            .status(RepoStatusCommand { name: args.name })
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
