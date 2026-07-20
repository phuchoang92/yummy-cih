//! Indexing / resolve-pattern admin MCP tools, split out of the `app.rs`
//! god-module. Merged via `+ Self::admin_router()` in `CihServer::new`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::CihServer;
use crate::args::{AddResolvePatternArgs, IndexRepoArgs, IndexStatusArgs, ListResolvePatternsArgs};
use crate::utils::json_result;
use crate::{indexing, patterns};

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
        indexing::index_repo(&self.backend, &self.falkor_url, &self.jobs, args).await
    }

    #[tool(
        description = "Poll the status of a repo-indexing job started by index_repo. \
            Returns status (running/done/failed), timing, and output or error message."
    )]
    async fn index_status(
        &self,
        Parameters(args): Parameters<IndexStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let jobs = self.jobs.read().await;
        match jobs.get(&args.job_id) {
            Some(state) => json_result(state),
            None => Err(McpError::invalid_params(
                format!(
                    "unknown job_id '{}' — use index_repo to start a job",
                    args.job_id
                ),
                None,
            )),
        }
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
        patterns::add_resolve_pattern(
            &self.backend,
            &self.falkor_url,
            &self.graph_key,
            &self.jobs,
            args,
        )
        .await
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
        patterns::list_resolve_patterns(&self.graph_key, args).await
    }
}
