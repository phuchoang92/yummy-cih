//! File-access MCP tools (`read_file`, `grep_files`), split out of the `app.rs`
//! tool god-module. The `#[tool_router(router = files_router)]` macro emits a
//! `files_router()` that `CihServer::new` merges into the dispatcher with
//! `+ Self::files_router()`. Handlers reach `CihServer`'s private fields/helpers
//! because this is a descendant module of `app`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::CihServer;
use crate::args::{GrepFilesArgs, ReadFileArgs};
use crate::files;

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
        files::read_file(&self.graph_key, self.read_file_limits, args).await
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
        files::grep_files(&self.graph_key, args).await
    }
}
