//! Indexing and resolve-pattern MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use crate::app::CihServer;
use crate::application::indexing::{
    CancelIndexCommand, IndexRepositoryCommand, IndexStatusCommand,
};
use crate::args::{
    AddResolvePatternArgs, IndexCancelArgs, IndexRepoArgs, IndexStatusArgs, ListResolvePatternsArgs,
};
use crate::patterns;
use crate::utils::{app_error_to_mcp, json_result};

#[tool_router(router = admin_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "Index a repository so its code graph becomes queryable by the other tools. \
            Runs scan → parse → resolve → load into the live FalkorDB graph. \
            Returns immediately with a `job_id`; use index_status(job_id=...) to poll for completion. \
            Typical time: 5–120 seconds depending on repo size. \
            An already-registered repo re-indexes under its own graph key; a NEW repo requires an \
            explicit `graph_key` (a fresh one — keys owned by other repos are rejected). \
            Example: index_repo(repo_path='/home/user/my-service')"
    )]
    async fn index_repo(
        &self,
        Parameters(args): Parameters<IndexRepoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command =
            IndexRepositoryCommand::try_new(args.repo_path, args.languages, args.graph_key)
                .map_err(app_error_to_mcp)?;
        let output = self
            .indexing_service()
            .start(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Poll the status of a repo-indexing job started by index_repo. \
            Returns status (queued/running/done/failed/timed_out/cancelled), timing, and \
            output or error message."
    )]
    async fn index_status(
        &self,
        Parameters(args): Parameters<IndexStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command = IndexStatusCommand::try_new(args.job_id).map_err(app_error_to_mcp)?;
        let output = self
            .indexing_service()
            .status(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Cancel a queued or running index job started by index_repo. A running \
            cih-engine process is killed; a queued job never starts. Returns immediately with \
            status \"cancelling\" — poll index_status(job_id=...) until the status settles as \
            \"cancelled\". Jobs that already finished cannot be cancelled."
    )]
    async fn index_cancel(
        &self,
        Parameters(args): Parameters<IndexCancelArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command = CancelIndexCommand::try_new(args.job_id).map_err(app_error_to_mcp)?;
        let output = self
            .indexing_service()
            .cancel(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Teach CIH a repository's own framework convention so its endpoints become \
            visible without any code change. Writes a rule to <repo>/cih.patterns.toml and \
            re-indexes. Use when route_map misses endpoints because the repo uses a custom/proprietary \
            annotation (e.g. @BankEndpoint(\"/pay\")) that the built-in Spring/JAX-RS detectors don't \
            know. First inspect the code (search_code/read_file) to find the annotation, its \
            path attribute, and any class-level prefix annotation, then call this. \
            kind=\"route\": annotation=the method annotation name (no @); path_attr=attribute holding \
            the URL (default \"value\"); method=fixed verb OR method_attr=attribute holding the verb; \
            class_prefix_annotation=optional class-level prefix annotation. Poll index_status with the \
            returned reindex_job_id, then re-run route_map."
    )]
    async fn add_resolve_pattern(
        &self,
        Parameters(args): Parameters<AddResolvePatternArgs>,
    ) -> Result<CallToolResult, McpError> {
        patterns::add_resolve_pattern(self.graph_key(), self.indexing_service(), args).await
    }

    #[tool(
        description = "List the custom resolve patterns (cih.patterns.toml) currently taught for a \
            repo. Use before add_resolve_pattern to avoid duplicates and see what conventions are \
            already recognized."
    )]
    async fn list_resolve_patterns(
        &self,
        Parameters(args): Parameters<ListResolvePatternsArgs>,
    ) -> Result<CallToolResult, McpError> {
        patterns::list_resolve_patterns(self.graph_key(), args).await
    }
}
