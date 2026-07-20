//! Cross-repo / contract MCP tools, split out of the `app.rs` tool god-module.
//! Merged into the dispatcher via `+ Self::crossrepo_router()` in `CihServer::new`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::CihServer;
use crate::args::{ApiImpactArgs, GroupContractsArgs, ShapeCheckArgs, TraceFlowXArgs};
use crate::contracts;

#[tool_router(router = crossrepo_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "Return cross-service contract matches for a repo group. \
        Run `cih-engine group sync <group>` first. Optional kind filter: \
        all, http/http_route, kafka/kafka_topic, spring/spring_event."
    )]
    async fn group_contracts(
        &self,
        Parameters(args): Parameters<GroupContractsArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::group_contracts(args, self.repo_context_provider()).await
    }

    #[tool(
        description = "Return all services that consume a given HTTP route across a repo group. \
        Path variables ({id}, :id) are normalized to wildcards for matching. \
        Run `cih-engine group sync <group>` first."
    )]
    async fn api_impact(
        &self,
        Parameters(args): Parameters<ApiImpactArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::api_impact(args, self.repo_context_provider(), &self.xflow).await
    }

    #[tool(
        description = "Cross-repo downstream trace: like trace_flow, but hops between repos \
        through the group's synced contract matches (HTTP consumer → provider route → handler; \
        Kafka publisher → listener). Walks each repo's graph artifacts; the entry point resolves \
        in the start repo — pass `repo` (a group member's registry name/path) to choose it, or \
        leave empty for the server's active graph key. Run `cih-engine group sync <group>` \
        first. Steps carry `repo` and `via.kind` (`CONTRACT` marks a cross-repo crossing)."
    )]
    async fn trace_flow_x(
        &self,
        Parameters(args): Parameters<TraceFlowXArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::trace_flow_x(args, self.repo_context_provider(), &self.xflow).await
    }

    #[tool(
        description = "Compare provider HTTP handler response DTO fields against consumer \
        field accesses for all shared HTTP contracts between two repos. \
        Re-run `cih-engine analyze` on both repos (to populate returnType), \
        then `cih-engine group sync <group>` before calling this."
    )]
    async fn shape_check(
        &self,
        Parameters(args): Parameters<ShapeCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        contracts::shape_check(args, self.repo_context_provider(), &self.artifacts).await
    }
}
