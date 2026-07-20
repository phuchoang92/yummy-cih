//! Testing, coverage, and taint MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::taint::TaintPathsCommand;
use crate::application::testing::{
    RegressionScopeCommand, TestCoverageCommand, UntestedPathsCommand,
};
use crate::args::{RegressionScopeArgs, TaintPathsArgs, TestCoverageArgs, UntestedPathsArgs};

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
        let output = self
            .testing_service()
            .test_coverage(TestCoverageCommand {
                repo: args.repo,
                name: args.name,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let output = self
            .testing_service()
            .regression_scope(RegressionScopeCommand {
                repo: args.repo,
                changed_files: args.changed_files,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Return production symbols (classes, methods) under a path prefix that \
            have no test coverage (no inbound TESTS edge). Helps identify coverage gaps."
    )]
    async fn untested_paths(
        &self,
        Parameters(args): Parameters<UntestedPathsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .testing_service()
            .untested_paths(UntestedPathsCommand {
                repo: args.repo,
                module_prefix: args.module_prefix,
                limit: (if args.limit == 0 { 50 } else { args.limit }).clamp(1, 500),
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let output = self
            .testing_service()
            .taint_paths(args.repo, command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
