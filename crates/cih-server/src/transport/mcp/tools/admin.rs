//! Repository catalog MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::admin::{
    ListReposPageCommand, RepoStatusCommand, LEGACY_LIST_REPOS_WIRE_BYTES,
};
use crate::transport::mcp::args::{ListReposArgs, ListReposPageArgs, StatusArgs};

#[tool_router(router = repository_admin_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "List all repos exactly when the registry fits the documented legacy \
        ceilings (200 entries and 256 KiB serialized MCP result). Larger registries return a \
        typed migration error; use list_repos_page instead."
    )]
    async fn list_repos(&self, _: Parameters<ListReposArgs>) -> Result<CallToolResult, McpError> {
        let output = self
            .repository_admin()
            .list_repos()
            .map_err(|error| legacy_list_repos_error(error.actual_count, None, error.count_cap))?;
        let result = json_result(&output)?;
        let wire_bytes = serde_json::to_vec(&result)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .len();
        if wire_bytes > LEGACY_LIST_REPOS_WIRE_BYTES {
            return Err(legacy_list_repos_error(
                output.repo_count(),
                Some(wire_bytes),
                crate::application::admin::LEGACY_LIST_REPOS_COUNT_CAP,
            ));
        }
        Ok(result)
    }

    #[tool(
        description = "List repository registry entries through stable v2 keyset pages. Supports \
        case-insensitive name/path filtering, defaults to 50 entries, caps at 200, includes \
        stale/missing status, and rejects a continuation if the registry changed."
    )]
    async fn list_repos_page(
        &self,
        Parameters(args): Parameters<ListReposPageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .repository_admin()
            .list_repos_page(ListReposPageCommand {
                filter: args.filter,
                limit: args.limit,
                cursor: args.cursor,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}

fn legacy_list_repos_error(
    actual_count: usize,
    actual_wire_bytes: Option<usize>,
    count_cap: usize,
) -> McpError {
    McpError::invalid_params(
        "legacy list_repos result exceeds its exact compatibility ceiling; use \
         list_repos_page(filter=\"\", limit=50)"
            .to_string(),
        Some(serde_json::json!({
            "code": "RESULT_TOO_LARGE",
            "operation": "list_repos",
            "replacement": "list_repos_page",
            "actual_count": actual_count,
            "count_cap": count_cap,
            "actual_wire_bytes": actual_wire_bytes,
            "wire_byte_cap": LEGACY_LIST_REPOS_WIRE_BYTES,
            "result_exact": false,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_overflow_is_a_typed_migration_error() {
        let error = legacy_list_repos_error(201, Some(300_000), 200);
        let data = error.data.expect("typed error data");
        assert_eq!(data["code"], "RESULT_TOO_LARGE");
        assert_eq!(data["replacement"], "list_repos_page");
        assert_eq!(data["actual_count"], 201);
        assert_eq!(data["wire_byte_cap"], LEGACY_LIST_REPOS_WIRE_BYTES);
    }
}
