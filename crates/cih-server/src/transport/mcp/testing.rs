//! Testing, coverage, and taint MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use crate::app::CihServer;
use crate::application::taint::TaintPathsCommand;
use crate::args::{RegressionScopeArgs, TaintPathsArgs, TestCoverageArgs, UntestedPathsArgs};
use crate::coverage;
use crate::utils::{app_error_to_mcp, json_result};

#[tool_router(router = testing_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "Return all test methods/classes that cover a symbol (via TESTS edges). \
            Helps a tester understand which tests exercise a given class or method."
    )]
    async fn test_coverage(
        &self,
        Parameters(args): Parameters<TestCoverageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rc = self.resolve(&args.repo).await?;
        coverage::test_coverage(&rc.store, args).await
    }

    #[tool(
        description = "Given a list of changed files, return all test classes that must be \
            re-run. Follows TESTS edges (direct + one-hop via CALLS). Use after `git diff \
            --name-only` to find the regression scope."
    )]
    async fn regression_scope(
        &self,
        Parameters(args): Parameters<RegressionScopeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rc = self.resolve(&args.repo).await?;
        coverage::regression_scope(&rc.store, args).await
    }

    #[tool(
        description = "Return production symbols (classes, methods) under a path prefix that \
            have no test coverage (no inbound TESTS edge). Helps identify coverage gaps."
    )]
    async fn untested_paths(
        &self,
        Parameters(args): Parameters<UntestedPathsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rc = self.resolve(&args.repo).await?;
        coverage::untested_paths(&rc.store, args).await
    }

    #[tool(
        description = "Find source→sink taint paths: user-controlled data entering through an \
            HTTP handler or event listener that reaches a dangerous operation. Categories: \
            `sql` (SQL injection), `exec` (OS command execution), `file` (unsafe file write), \
            `html` (XSS). Runs inter-procedural BFS on the call graph; pass refine=true for \
            slower flow-sensitive CFG/PDG confirmation with adjusted confidence. Sink rules \
            can be extended via `cih.taint.toml` in the repo root."
    )]
    async fn taint_paths(
        &self,
        Parameters(args): Parameters<TaintPathsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let command =
            TaintPathsCommand::try_new(args.category, args.min_confidence, args.refine, args.limit)
                .map_err(app_error_to_mcp)?;
        let repo = self.resolve_repo(&args.repo)?;
        let output = self
            .taint_service()
            .taint_paths(repo, command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
