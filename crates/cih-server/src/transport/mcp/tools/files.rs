//! File-access MCP adapters (`read_file`, `grep_files`).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::files::{GrepFilesCommand, ReadFileCommand};
use crate::args::{GrepFilesArgs, ReadFileArgs};

#[tool_router(router = files_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "Read the source of a file in the repo. Use the `file` field from \
            search_code or context results as the `path`. Optionally slice with start_line / \
            end_line (1-based, inclusive) to fetch only the relevant section. Files over the \
            size limit are rejected, and un-ranged reads are capped; when capped the response \
            sets `truncated: true` — pass start_line/end_line to read further."
    )]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .file_service()
            .read_file(ReadFileCommand {
                repo: args.repo,
                path: args.path,
                start_line: args.start_line,
                end_line: args.end_line,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Search for a regex pattern across source files in the repo. \
            Use this to find comments, TODOs, annotations, string literals, or any \
            text not captured by the graph index. Prefix the pattern with (?i) for \
            case-insensitive search. `glob` filters by file path \
            (e.g. \"**/*.java\", \"src/**/*.rs\"). Returns up to `limit` matches (default 200)."
    )]
    async fn grep_files(
        &self,
        Parameters(args): Parameters<GrepFilesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .file_service()
            .grep_files(GrepFilesCommand {
                repo: args.repo,
                pattern: args.pattern,
                glob: args.glob,
                limit: args.limit,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
