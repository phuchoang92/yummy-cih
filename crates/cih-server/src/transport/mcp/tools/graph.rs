//! Graph analysis MCP adapters.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{model::CallToolResult, tool, tool_router, ErrorData as McpError};

use super::super::error::{app_error_to_mcp, json_result, json_result_compatible, text_result};
use super::super::CihServer;
use crate::application::change_detection::DetectChangesCommand;
use crate::application::graph::{
    CommunitiesCommand, ComplexityHotspotsCommand, ContextCommand, DetectChangesForRepoCommand,
    FindDuplicatesCommand, ImpactCommand, ReachesCommand, RouteMapCommand, SymbolQueryOutput,
    TraceFlowCommand,
};
use crate::ports::changed_files_source::ChangeScope;
use crate::transport::mcp::args::{
    CommunitiesArgs, CommunitiesFormat, ComplexityHotspotsArgs, ContextArgs, DetectChangesArgs,
    DiffScope, FindDuplicatesArgs, ImpactArgs, ImpactFormat, ReachesArgs, RouteMapArgs,
    RouteMapFormat, TraceFlowArgs, TraceFlowFormat,
};
use crate::viz::{render_community_diagram, render_d3_impact, render_mermaid_flow, render_openapi};

#[tool_router(router = graph_router, vis = "pub(crate)")]
impl CihServer {
    #[tool(
        description = "360° context for a symbol: its node, callers, callees, and processes. \
        Callers, callees, and processes are independently paged (default 100, maximum 500); \
        follow each section's own next_cursor. \
        Pass a full NodeId (e.g. `Class:com.acme.OrderService`) or a short name; \
        short names return {\"status\":\"ambiguous\",\"candidates\":[...]} when multiple match."
    )]
    async fn context(
        &self,
        Parameters(args): Parameters<ContextArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .graph_queries()
            .context(ContextCommand {
                repo: args.repo,
                name: args.name,
                caller_limit: args.caller_limit,
                callee_limit: args.callee_limit,
                process_limit: args.process_limit,
                caller_cursor: args.caller_cursor,
                callee_cursor: args.callee_cursor,
                process_cursor: args.process_cursor,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Blast radius of changing a symbol: affected symbols, depth, and risk. \
        Pass a full NodeId or short name; short names that match multiple symbols return \
        {\"status\":\"ambiguous\",\"candidates\":[...]}."
    )]
    async fn impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, McpError> {
        let format = args.format;
        let output = self
            .graph_queries()
            .impact(ImpactCommand {
                repo: args.repo,
                name: args.name,
                direction: args.direction.into(),
                max_depth: if args.max_depth == 0 {
                    4
                } else {
                    args.max_depth
                },
            })
            .await
            .map_err(app_error_to_mcp)?;
        if format == ImpactFormat::Diagram {
            if let SymbolQueryOutput::Resolved(impact) = &output {
                return json_result(&render_d3_impact(&impact.impact));
            }
        }
        json_result(&output)
    }

    #[tool(description = "List community clusters detected in the codebase.")]
    async fn communities(
        &self,
        Parameters(args): Parameters<CommunitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let diagram = args.format == CommunitiesFormat::Diagram;
        let output = self
            .graph_queries()
            .communities(CommunitiesCommand {
                repo: args.repo,
                limit: (args.limit > 0).then_some(args.limit),
                include_edges: diagram,
            })
            .await
            .map_err(app_error_to_mcp)?;
        if diagram {
            return json_result(&render_community_diagram(
                &output.communities,
                &output.edges,
            ));
        }
        json_result_compatible(&output.communities, &output)
    }

    #[tool(
        description = "List HTTP/REST endpoints discovered in the indexed repo. \
        Supported frameworks: Spring MVC/Boot (@GetMapping etc.), NestJS (@Get/@Controller), \
        Flask (@app.route, @bp.route, Blueprint shorthand), FastAPI (@router.get etc. with APIRouter prefix), \
        Express (router.get/post/put/delete/patch). \
        V1 limitation: cross-file router mounts (include_router/register_blueprint with prefix in the \
        app entry-point) are not resolved — routes show their per-file prefix only. \
        Use prefix to filter by path prefix (e.g. prefix=\"/api/users\")."
    )]
    async fn route_map(
        &self,
        Parameters(args): Parameters<RouteMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        let format = args.format;
        let output = self
            .graph_queries()
            .routes(RouteMapCommand {
                repo: args.repo,
                prefix: (!args.prefix.is_empty()).then_some(args.prefix),
                limit: (if args.limit == 0 { 200 } else { args.limit }).clamp(1, 1000),
            })
            .await
            .map_err(app_error_to_mcp)?;
        if format == RouteMapFormat::Openapi {
            return json_result(&render_openapi(&output.routes));
        }
        json_result_compatible(&output.routes, &output)
    }

    #[tool(
        description = "Diff-driven change-impact analysis. Runs `git diff` in the repo, \
        maps changed files to graph nodes, traces upstream blast radius via BFS, and \
        scores overall risk (low/medium/high/critical). \
        scope: `working` (all uncommitted), `staged` (index only), `base_ref` (vs a branch/SHA)."
    )]
    async fn detect_changes(
        &self,
        Parameters(args): Parameters<DetectChangesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let scope = match args.scope {
            DiffScope::Working => ChangeScope::Working,
            DiffScope::Staged => ChangeScope::Staged,
            DiffScope::BaseRef => ChangeScope::BaseRef,
        };
        let analysis =
            DetectChangesCommand::try_new(scope, args.base_ref).map_err(app_error_to_mcp)?;
        let output = self
            .graph_queries()
            .detect_changes(DetectChangesForRepoCommand {
                repo: args.repo,
                analysis,
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Trace the downstream execution chain from an HTTP route or method: \
            controller → services → repos → external HTTP calls and events. \
            Traverses CALLS, HANDLES_ROUTE, EXTERNAL_CALL, PUBLISHES_EVENT, LISTENS_TO edges. \
            Pass a Route node id (e.g. `Route:GET /api/checkout`) or a method id."
    )]
    async fn trace_flow(
        &self,
        Parameters(args): Parameters<TraceFlowArgs>,
    ) -> Result<CallToolResult, McpError> {
        let format = args.format;
        let output = self
            .graph_queries()
            .trace_flow(TraceFlowCommand {
                repo: args.repo,
                entry_point: args.entry_point,
                max_depth: (if args.max_depth == 0 {
                    6
                } else {
                    args.max_depth
                })
                .clamp(1, 10),
                exclude_kinds: args.exclude_kinds,
                business_only: args.business_only,
                max_nodes: (if args.max_nodes == 0 {
                    100
                } else {
                    args.max_nodes
                })
                .clamp(1, 500) as usize,
                offset: args.offset as usize,
            })
            .await
            .map_err(app_error_to_mcp)?;
        if format == TraceFlowFormat::Mermaid {
            if let SymbolQueryOutput::Resolved(flow) = &output {
                let mut rendered = render_mermaid_flow(&flow.entry_point, &flow.steps);
                if let Some(next) = flow.next_offset {
                    rendered.push_str(&format!("\n%% truncated — continue with offset={next}"));
                }
                return text_result(rendered);
            }
        }
        json_result(&output)
    }

    #[tool(
        description = "Answer 'does X reach Y?': shortest evidence paths from an entry symbol \
            to a target symbol or database table, across call, route, messaging, and SQL edges. \
            To prove a table write, pass access=`write`; `read` and `write` require a DbTable \
            target. Results distinguish reachable, not_reachable, and inconclusive."
    )]
    async fn reaches(
        &self,
        Parameters(args): Parameters<ReachesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .graph_queries()
            .reaches(ReachesCommand {
                repo: args.repo,
                from: args.from,
                to: args.to,
                max_depth: (if args.max_depth == 0 {
                    8
                } else {
                    args.max_depth
                })
                .clamp(1, 12),
                max_paths: (if args.max_paths == 0 {
                    3
                } else {
                    args.max_paths
                })
                .clamp(1, 10) as usize,
                access: args.access.into(),
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Return methods with high complexity. Finds methods that are hard to test \
            or maintain: filters by cyclomatic complexity (branches), cognitive complexity \
            (readability penalty), and transitive loop depth (additive nesting through call chain). \
            Use to prioritize refactoring targets."
    )]
    async fn complexity_hotspots(
        &self,
        Parameters(args): Parameters<ComplexityHotspotsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .graph_queries()
            .complexity_hotspots(ComplexityHotspotsCommand {
                repo: args.repo,
                min_cyclomatic: (args.min_cyclomatic > 0).then_some(args.min_cyclomatic),
                min_cognitive: (args.min_cognitive > 0).then_some(args.min_cognitive),
                min_transitive_loop: (args.min_transitive_loop > 0)
                    .then_some(args.min_transitive_loop),
                limit: if args.limit == 0 {
                    20
                } else {
                    args.limit.min(500)
                },
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }

    #[tool(
        description = "Find near-duplicate methods (MinHash similarity >= threshold). \
            Identifies copy-paste code across the codebase. Returns candidates with Jaccard \
            similarity score and file path. Use to detect inconsistently-maintained duplicates."
    )]
    async fn find_duplicates(
        &self,
        Parameters(args): Parameters<FindDuplicatesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let output = self
            .graph_queries()
            .find_duplicates(FindDuplicatesCommand {
                repo: args.repo,
                name: args.name,
                min_jaccard: if args.min_jaccard == 0.0 {
                    0.95
                } else {
                    args.min_jaccard
                },
                limit: if args.limit == 0 { 10 } else { args.limit },
            })
            .await
            .map_err(app_error_to_mcp)?;
        json_result(&output)
    }
}
