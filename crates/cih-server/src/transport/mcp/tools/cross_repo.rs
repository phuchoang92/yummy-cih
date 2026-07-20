//! Cross-repository contract MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result};
use super::super::CihServer;
use crate::application::contracts::{
    ApiImpactCommand, GroupContractsCommand, ShapeCheckCommand, TraceFlowXCommand,
};
use crate::transport::mcp::args::{
    ApiImpactArgs, GroupContractsArgs, ShapeCheckArgs, TraceFlowXArgs,
};

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
        let command =
            GroupContractsCommand::try_new(args.group, args.kind).map_err(app_error_to_mcp)?;
        let output = self
            .contract_service()
            .group_contracts(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let command = ApiImpactCommand::try_new(
            args.group,
            args.method,
            args.path,
            args.include_callers,
            args.caller_depth,
        )
        .map_err(app_error_to_mcp)?;
        let output = self
            .contract_service()
            .api_impact(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let command = TraceFlowXCommand::try_new(
            args.entry_point,
            args.repo,
            args.group,
            args.max_depth,
            args.max_hops,
        )
        .map_err(app_error_to_mcp)?;
        let output = self
            .contract_service()
            .trace_flow_x(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
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
        let command = ShapeCheckCommand::try_new(args.group, args.provider, args.consumer)
            .map_err(app_error_to_mcp)?;
        let output = self
            .contract_service()
            .shape_check(command)
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
