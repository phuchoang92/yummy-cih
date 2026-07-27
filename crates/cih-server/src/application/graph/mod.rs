//! Repository-scoped graph query use cases.

use cih_core::{Node, NodeId, NodeKind};
use cih_graph_store::{
    CommunityEdge, CommunityInfo, ContextCursorKey, ContextFilter, ContextPage, ContextSection,
    DbEffect, Direction, FlowFilter, FlowHop, GraphStoreError, HotspotNode, Impact, PathAccess,
    PathFilter, RouteInfo, SimilarMethod, TraversalStats, CONTEXT_DEFAULT_LIMIT, CONTEXT_MAX_LIMIT,
    FLOW_VISIBLE_WINDOW,
};
use serde::{Deserialize, Serialize};

use crate::application::app_services::RepoContextService;
use crate::application::change_detection::{
    ChangeDetectionService, DetectChangesCommand, DetectChangesOutput,
};
use crate::application::cursor::{
    canonical_filter_hash, unix_now, CursorCodec, RepositoryCursorBinding, DEFAULT_CURSOR_TTL_SECS,
};
use crate::domain::completeness::ResultBounds;
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;

#[derive(Clone)]
pub(crate) struct GraphQueryService {
    repos: RepoContextService,
    change_detection: ChangeDetectionService,
}

impl GraphQueryService {
    pub(crate) fn new(repos: RepoContextService, change_detection: ChangeDetectionService) -> Self {
        Self {
            repos,
            change_detection,
        }
    }

    pub(crate) async fn context(
        &self,
        command: ContextCommand,
    ) -> Result<SymbolQueryOutput<ContextOutput>, AppError> {
        let caller_limit = context_limit("caller_limit", command.caller_limit)?;
        let callee_limit = context_limit("callee_limit", command.callee_limit)?;
        let process_limit = context_limit("process_limit", command.process_limit)?;
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let cursor_codec = CursorCodec::global()?;
        let cursor_now = unix_now();
        let repository = RepositoryCursorBinding::from_entry(&repo.repo.registry_entry);
        match resolve_symbol(&repo.store, &command.name).await? {
            SymbolResolution::Id(id) => {
                let filter = ContextFilter {
                    caller_limit,
                    callee_limit,
                    process_limit,
                    caller_after: decode_context_cursor(
                        command.caller_cursor.as_deref(),
                        "callers",
                        &id,
                        caller_limit,
                        &repository,
                        cursor_codec,
                        cursor_now,
                    )?,
                    callee_after: decode_context_cursor(
                        command.callee_cursor.as_deref(),
                        "callees",
                        &id,
                        callee_limit,
                        &repository,
                        cursor_codec,
                        cursor_now,
                    )?,
                    process_after: decode_context_cursor(
                        command.process_cursor.as_deref(),
                        "processes",
                        &id,
                        process_limit,
                        &repository,
                        cursor_codec,
                        cursor_now,
                    )?,
                };
                let page = repo
                    .store
                    .context_page(&id, &filter)
                    .await
                    .map_err(graph_error)?;
                Ok(SymbolQueryOutput::Resolved(ContextOutput::from_page(
                    page,
                    ContextPageShape {
                        caller_limit,
                        callee_limit,
                        process_limit,
                        caller_offset: filter.caller_after.is_some(),
                        callee_offset: filter.callee_after.is_some(),
                        process_offset: filter.process_after.is_some(),
                    },
                    ContextCursorScope {
                        repository: &repository,
                        codec: cursor_codec,
                        now: cursor_now,
                    },
                )?))
            }
            SymbolResolution::Ambiguous(nodes) => Ok(SymbolQueryOutput::Ambiguous(
                AmbiguousResult::from_nodes(nodes),
            )),
            SymbolResolution::NotFound => Err(symbol_not_found(command.name)),
        }
    }

    pub(crate) async fn impact(
        &self,
        command: ImpactCommand,
    ) -> Result<SymbolQueryOutput<ImpactOutput>, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        match resolve_symbol(&repo.store, &command.name).await? {
            SymbolResolution::Id(id) => repo
                .store
                .impact(&id, command.direction, command.max_depth)
                .await
                .map(|impact| {
                    let completeness = impact_bounds(&impact);
                    SymbolQueryOutput::Resolved(ImpactOutput {
                        completeness,
                        impact,
                    })
                })
                .map_err(graph_error),
            SymbolResolution::Ambiguous(nodes) => Ok(SymbolQueryOutput::Ambiguous(
                AmbiguousResult::from_nodes(nodes),
            )),
            SymbolResolution::NotFound => Err(symbol_not_found(command.name)),
        }
    }

    pub(crate) async fn communities(
        &self,
        command: CommunitiesCommand,
    ) -> Result<CommunitiesOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let mut communities = repo.store.communities().await.map_err(graph_error)?;
        let total = communities.len();
        if let Some(limit) = command.limit {
            communities.truncate(limit);
        }
        let completeness = ResultBounds::exact_limit(total, communities.len(), command.limit);
        let edges = if command.include_edges {
            repo.store.community_graph().await.map_err(graph_error)?
        } else {
            Vec::new()
        };
        Ok(CommunitiesOutput {
            communities,
            edges,
            completeness,
        })
    }

    pub(crate) async fn routes(
        &self,
        command: RouteMapCommand,
    ) -> Result<RouteMapOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        // Over-fetch by one: the store applies a hard `LIMIT` and returns no total,
        // so without this probe route_map could never report `complete` and a capped
        // list would look authoritative. The extra row proves whether more exist;
        // truncate it back off before returning.
        let limit = command.limit;
        let mut routes = repo
            .store
            .route_map(command.prefix.as_deref(), limit.saturating_add(1))
            .await
            .map_err(graph_error)?;
        let has_more = routes.len() > limit;
        routes.truncate(limit);
        let completeness = if has_more {
            ResultBounds::backend_limited(routes.len(), limit)
        } else {
            ResultBounds::exact_limit(routes.len(), routes.len(), Some(limit))
        };
        Ok(RouteMapOutput {
            routes,
            completeness,
        })
    }

    pub(crate) async fn trace_flow(
        &self,
        command: TraceFlowCommand,
    ) -> Result<SymbolQueryOutput<TraceFlowOutput>, AppError> {
        let window_end = command
            .offset
            .checked_add(command.max_nodes)
            .ok_or_else(|| AppError::InvalidInput {
                field: "offset",
                message: "offset + max_nodes overflowed".to_string(),
            })?;
        if window_end > FLOW_VISIBLE_WINDOW {
            return Err(AppError::InvalidInput {
                field: "offset",
                message: format!(
                    "offset + max_nodes must be at most {FLOW_VISIBLE_WINDOW} (got {window_end})"
                ),
            });
        }
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let mut exclude_kinds = parse_node_kinds(&command.exclude_kinds)?;
        if command.business_only && !exclude_kinds.contains(&NodeKind::Constructor) {
            exclude_kinds.push(NodeKind::Constructor);
        }
        let filter = FlowFilter {
            max_depth: command.max_depth,
            exclude_kinds,
            exclude_accessors: command.business_only,
            limit: command.max_nodes,
            offset: command.offset,
        };
        match resolve_symbol(&repo.store, &command.entry_point).await? {
            SymbolResolution::Id(id) => {
                let page = repo
                    .store
                    .flow_downstream(&id, &filter)
                    .await
                    .map_err(graph_error)?;
                let steps = page.hops;
                // Surface table reads/writes of every traced callable: side effects
                // like an audit INSERT are what a trace is usually asked to prove.
                // Best-effort — a db_effects failure must not sink the trace itself.
                let method_ids: Vec<NodeId> = steps
                    .iter()
                    .map(|hop| hop.node.id.clone())
                    .filter(|id| {
                        id.as_str().starts_with("Method:")
                            || id.as_str().starts_with("Constructor:")
                    })
                    .collect();
                let (db_effects, db_effects_complete) = match repo
                    .store
                    .db_effects_for_methods(&method_ids)
                    .await
                {
                    Ok(effects) => (effects, true),
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "trace_flow: DB-effect evidence unavailable — returning empty db_effects with db_effects_complete=false"
                        );
                        (Vec::new(), false)
                    }
                };
                let next_offset = (page.has_more && !page.traversal.truncated)
                    .then(|| command.offset.checked_add(steps.len()))
                    .flatten()
                    .filter(|offset| *offset < FLOW_VISIBLE_WINDOW)
                    .and_then(|offset| u32::try_from(offset).ok());
                let completeness = ResultBounds::paged(
                    steps.len(),
                    command.offset,
                    page.has_more,
                    filter.effective_limit(),
                    page.traversal.truncated,
                    db_effects_complete,
                );
                Ok(SymbolQueryOutput::Resolved(TraceFlowOutput {
                    entry_point: id,
                    depth_limit: command.max_depth,
                    step_count: steps.len(),
                    completeness,
                    next_offset,
                    db_effects,
                    db_effects_complete,
                    traversal: page.traversal,
                    steps,
                }))
            }
            SymbolResolution::Ambiguous(nodes) => Ok(SymbolQueryOutput::Ambiguous(
                AmbiguousResult::from_nodes(nodes),
            )),
            SymbolResolution::NotFound => Err(symbol_not_found(command.entry_point)),
        }
    }

    /// "Does X reach Y?" — shortest evidence paths from an entry symbol to a
    /// target symbol or table over call + side-effect edges.
    pub(crate) async fn reaches(
        &self,
        command: ReachesCommand,
    ) -> Result<SymbolQueryOutput<ReachesOutput>, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let from = match resolve_symbol(&repo.store, &command.from).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(nodes) => {
                return Ok(SymbolQueryOutput::Ambiguous(AmbiguousResult::from_nodes(
                    nodes,
                )))
            }
            SymbolResolution::NotFound => return Err(symbol_not_found(command.from)),
        };
        let to = match resolve_symbol(&repo.store, &command.to).await? {
            SymbolResolution::Id(id) => id,
            SymbolResolution::Ambiguous(nodes) => {
                return Ok(SymbolQueryOutput::Ambiguous(AmbiguousResult::from_nodes(
                    nodes,
                )))
            }
            // Bare target names are often table names (`audit_log`) — try the
            // DbTable id (table names are stored uppercase) before giving up.
            SymbolResolution::NotFound if !command.to.contains(':') => {
                let table = NodeId::new(format!("DbTable:{}", command.to.to_uppercase()));
                match repo.store.get_node(&table).await.map_err(graph_error)? {
                    Some(node) => node.id,
                    None => return Err(symbol_not_found(command.to)),
                }
            }
            SymbolResolution::NotFound => return Err(symbol_not_found(command.to)),
        };
        let page = repo
            .store
            .paths_between(
                &from,
                &to,
                &PathFilter {
                    max_depth: command.max_depth,
                    max_paths: command.max_paths,
                    access: command.access,
                },
            )
            .await
            .map_err(graph_error)?;
        let status = reaches_status(!page.paths.is_empty(), page.traversal.truncated);
        let completeness = ResultBounds::traversal(
            page.paths.len(),
            page.has_more,
            page.traversal.truncated,
            command.max_paths,
        );
        Ok(SymbolQueryOutput::Resolved(ReachesOutput {
            reachable: status == ReachesStatus::Reachable,
            status,
            access: command.access,
            completeness,
            traversal: page.traversal,
            from,
            to,
            paths: page.paths,
        }))
    }

    pub(crate) async fn complexity_hotspots(
        &self,
        command: ComplexityHotspotsCommand,
    ) -> Result<ComplexityHotspotsOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let hotspots = repo
            .store
            .complexity_hotspots(
                command.min_cyclomatic,
                command.min_cognitive,
                command.min_transitive_loop,
                command.limit,
            )
            .await
            .map_err(graph_error)?;
        Ok(ComplexityHotspotsOutput {
            count: hotspots.len(),
            completeness: ResultBounds::backend_limited(hotspots.len(), command.limit),
            hotspots,
        })
    }

    pub(crate) async fn find_duplicates(
        &self,
        command: FindDuplicatesCommand,
    ) -> Result<SymbolQueryOutput<FindDuplicatesOutput>, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        match resolve_symbol(&repo.store, &command.name).await? {
            SymbolResolution::Id(id) => {
                let similar = repo
                    .store
                    .similar_methods(&id, command.min_jaccard, command.limit)
                    .await
                    .map_err(graph_error)?;
                Ok(SymbolQueryOutput::Resolved(FindDuplicatesOutput {
                    query_id: id,
                    min_jaccard: command.min_jaccard,
                    count: similar.len(),
                    completeness: ResultBounds::backend_limited(similar.len(), command.limit),
                    similar,
                }))
            }
            SymbolResolution::Ambiguous(nodes) => Ok(SymbolQueryOutput::Ambiguous(
                AmbiguousResult::from_nodes(nodes),
            )),
            SymbolResolution::NotFound => Err(symbol_not_found(command.name)),
        }
    }

    pub(crate) async fn detect_changes(
        &self,
        command: DetectChangesForRepoCommand,
    ) -> Result<DetectChangesOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        self.change_detection.execute(&repo, command.analysis).await
    }
}

pub(crate) struct ContextCommand {
    pub(crate) repo: String,
    pub(crate) name: String,
    pub(crate) caller_limit: usize,
    pub(crate) callee_limit: usize,
    pub(crate) process_limit: usize,
    pub(crate) caller_cursor: Option<String>,
    pub(crate) callee_cursor: Option<String>,
    pub(crate) process_cursor: Option<String>,
}

pub(crate) struct ImpactCommand {
    pub(crate) repo: String,
    pub(crate) name: String,
    pub(crate) direction: Direction,
    pub(crate) max_depth: u32,
}

pub(crate) struct CommunitiesCommand {
    pub(crate) repo: String,
    pub(crate) limit: Option<usize>,
    pub(crate) include_edges: bool,
}

pub(crate) struct RouteMapCommand {
    pub(crate) repo: String,
    pub(crate) prefix: Option<String>,
    pub(crate) limit: usize,
}

pub(crate) struct TraceFlowCommand {
    pub(crate) repo: String,
    pub(crate) entry_point: String,
    pub(crate) max_depth: u32,
    /// Node-kind labels to hide from hops (validated against [`NodeKind`]).
    pub(crate) exclude_kinds: Vec<String>,
    /// Business-logic view: also hides constructors and trivial accessors.
    pub(crate) business_only: bool,
    /// Page size (already clamped by the transport adapter).
    pub(crate) max_nodes: usize,
    /// Continuation offset from a prior page's `next_offset`.
    pub(crate) offset: usize,
}

pub(crate) struct ReachesCommand {
    pub(crate) repo: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) max_depth: u32,
    pub(crate) max_paths: usize,
    pub(crate) access: PathAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReachesStatus {
    Reachable,
    NotReachable,
    Inconclusive,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReachesOutput {
    pub(crate) from: NodeId,
    pub(crate) to: NodeId,
    /// True when at least one path exists within the depth budget. False means
    /// "not reachable within max_depth over indexed edges" — not proof of no
    /// runtime path.
    pub(crate) reachable: bool,
    pub(crate) status: ReachesStatus,
    pub(crate) access: PathAccess,
    pub(crate) completeness: ResultBounds,
    pub(crate) traversal: TraversalStats,
    pub(crate) paths: Vec<cih_graph_store::PathInfo>,
}

pub(crate) struct ComplexityHotspotsCommand {
    pub(crate) repo: String,
    pub(crate) min_cyclomatic: Option<u16>,
    pub(crate) min_cognitive: Option<u16>,
    pub(crate) min_transitive_loop: Option<u8>,
    pub(crate) limit: usize,
}

pub(crate) struct FindDuplicatesCommand {
    pub(crate) repo: String,
    pub(crate) name: String,
    pub(crate) min_jaccard: f32,
    pub(crate) limit: usize,
}

pub(crate) struct DetectChangesForRepoCommand {
    pub(crate) repo: String,
    pub(crate) analysis: DetectChangesCommand,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum SymbolQueryOutput<T> {
    Resolved(T),
    Ambiguous(AmbiguousResult),
}

#[derive(Debug, Serialize)]
pub(crate) struct AmbiguousCandidate {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) file: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AmbiguousResult {
    pub(crate) status: &'static str,
    pub(crate) candidates: Vec<AmbiguousCandidate>,
}

impl AmbiguousResult {
    pub(crate) fn from_nodes(nodes: Vec<Node>) -> Self {
        Self {
            status: "ambiguous",
            candidates: nodes
                .into_iter()
                .map(|node| AmbiguousCandidate {
                    id: node.id.to_string(),
                    kind: node.kind.label().to_string(),
                    name: node.name,
                    file: node.file,
                })
                .collect(),
        }
    }
}

const CONTEXT_CURSOR_OPERATION: &str = "context";
const CONTEXT_CURSOR_FILTER: &[u8] = b"context-section-filter-v1:none";

#[derive(Debug, Serialize, Deserialize)]
struct ContextCursorPayload {
    section: String,
    symbol: String,
    limit: usize,
    filter_hash: String,
    repository: RepositoryCursorBinding,
    key: ContextCursorKey,
}

#[derive(Clone, Copy)]
struct ContextPageShape {
    caller_limit: usize,
    callee_limit: usize,
    process_limit: usize,
    caller_offset: bool,
    callee_offset: bool,
    process_offset: bool,
}

#[derive(Clone, Copy)]
struct ContextCursorScope<'a> {
    repository: &'a RepositoryCursorBinding,
    codec: &'a CursorCodec,
    now: u64,
}

#[derive(Clone, Copy)]
struct ContextSectionSpec<'a> {
    name: &'a str,
    limit: usize,
    page_offset: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ContextSectionOutput {
    pub(crate) returned: usize,
    pub(crate) limit: usize,
    pub(crate) page_offset: bool,
    pub(crate) complete: bool,
    pub(crate) has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ContextOutput {
    pub(crate) node: Node,
    /// Legacy-compatible arrays remain at their original locations.
    pub(crate) callers: Vec<Node>,
    pub(crate) callees: Vec<Node>,
    pub(crate) processes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) community: Option<CommunityInfo>,
    pub(crate) callers_page: ContextSectionOutput,
    pub(crate) callees_page: ContextSectionOutput,
    pub(crate) processes_page: ContextSectionOutput,
}

impl ContextOutput {
    fn from_page(
        page: ContextPage,
        shape: ContextPageShape,
        cursor_scope: ContextCursorScope<'_>,
    ) -> Result<Self, AppError> {
        let symbol = page.node.id.clone();
        let callers_page = context_section_output(
            ContextSectionSpec {
                name: "callers",
                limit: shape.caller_limit,
                page_offset: shape.caller_offset,
            },
            &symbol,
            &page.callers,
            cursor_scope,
        )?;
        let callees_page = context_section_output(
            ContextSectionSpec {
                name: "callees",
                limit: shape.callee_limit,
                page_offset: shape.callee_offset,
            },
            &symbol,
            &page.callees,
            cursor_scope,
        )?;
        let processes_page = context_section_output(
            ContextSectionSpec {
                name: "processes",
                limit: shape.process_limit,
                page_offset: shape.process_offset,
            },
            &symbol,
            &page.processes,
            cursor_scope,
        )?;
        Ok(Self {
            node: page.node,
            callers: page.callers.items,
            callees: page.callees.items,
            processes: page.processes.items,
            community: page.community,
            callers_page,
            callees_page,
            processes_page,
        })
    }
}

fn context_section_output<T>(
    spec: ContextSectionSpec<'_>,
    symbol: &NodeId,
    section: &ContextSection<T>,
    cursor_scope: ContextCursorScope<'_>,
) -> Result<ContextSectionOutput, AppError> {
    let next_cursor = section
        .next
        .as_ref()
        .map(|key| {
            encode_context_cursor(
                spec.name,
                symbol,
                spec.limit,
                cursor_scope.repository,
                key,
                cursor_scope.codec,
                cursor_scope.now,
            )
        })
        .transpose()?;
    Ok(ContextSectionOutput {
        returned: section.items.len(),
        limit: spec.limit,
        page_offset: spec.page_offset,
        complete: !spec.page_offset && !section.has_more,
        has_more: section.has_more,
        next_cursor,
    })
}

fn context_limit(field: &'static str, requested: usize) -> Result<usize, AppError> {
    let limit = if requested == 0 {
        CONTEXT_DEFAULT_LIMIT
    } else {
        requested
    };
    if limit > CONTEXT_MAX_LIMIT {
        return Err(AppError::InvalidInput {
            field,
            message: format!("must be at most {CONTEXT_MAX_LIMIT}"),
        });
    }
    Ok(limit)
}

fn encode_context_cursor(
    section: &str,
    symbol: &NodeId,
    limit: usize,
    repository: &RepositoryCursorBinding,
    key: &ContextCursorKey,
    cursor_codec: &CursorCodec,
    now: u64,
) -> Result<String, AppError> {
    cursor_codec
        .encode_at(
            CONTEXT_CURSOR_OPERATION,
            DEFAULT_CURSOR_TTL_SECS,
            &ContextCursorPayload {
                section: section.to_string(),
                symbol: symbol.to_string(),
                limit,
                filter_hash: context_filter_hash(),
                repository: repository.clone(),
                key: key.clone(),
            },
            now,
        )
        .map_err(|error| error.into_app_error("context_cursor"))
}

fn decode_context_cursor(
    raw: Option<&str>,
    expected_section: &str,
    expected_symbol: &NodeId,
    expected_limit: usize,
    expected_repository: &RepositoryCursorBinding,
    cursor_codec: &CursorCodec,
    now: u64,
) -> Result<Option<ContextCursorKey>, AppError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let cursor: ContextCursorPayload = cursor_codec
        .decode_at(raw, CONTEXT_CURSOR_OPERATION, now)
        .map_err(|error| error.into_app_error("context_cursor"))?;
    if cursor.section != expected_section {
        return Err(context_cursor_error(
            "wrong_section",
            format!("cursor is not valid for the {expected_section} context section"),
        ));
    }
    if cursor.symbol != expected_symbol.as_str() {
        return Err(context_cursor_error(
            "wrong_symbol",
            "cursor belongs to a different resolved symbol; restart from the first page",
        ));
    }
    if cursor.repository.identity != expected_repository.identity {
        return Err(context_cursor_error(
            "wrong_repository",
            "cursor belongs to a different repository; restart from the first page",
        ));
    }
    if cursor.repository.published_epoch != expected_repository.published_epoch
        || cursor.repository.published_graph_content_version
            != expected_repository.published_graph_content_version
    {
        return Err(context_cursor_error(
            "generation_changed",
            "repository graph generation changed between pages; restart from the first page",
        ));
    }
    if cursor.limit != expected_limit {
        return Err(context_cursor_error(
            "wrong_page_bounds",
            "cursor limit differs from this section request; reuse the original limit",
        ));
    }
    if cursor.filter_hash != context_filter_hash() {
        return Err(context_cursor_error(
            "wrong_filter",
            "cursor filter differs from this section request; restart from the first page",
        ));
    }
    Ok(Some(cursor.key))
}

fn context_filter_hash() -> String {
    canonical_filter_hash(CONTEXT_CURSOR_FILTER)
}

fn context_cursor_error(code: &'static str, message: impl Into<String>) -> AppError {
    AppError::InvalidInput {
        field: "context_cursor",
        message: format!("{code}: {}", message.into()),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CommunitiesOutput {
    pub(crate) communities: Vec<CommunityInfo>,
    pub(crate) edges: Vec<CommunityEdge>,
    pub(crate) completeness: ResultBounds,
}

#[derive(Debug, Serialize)]
pub(crate) struct RouteMapOutput {
    pub(crate) routes: Vec<RouteInfo>,
    pub(crate) completeness: ResultBounds,
}

#[derive(Debug, Serialize)]
pub(crate) struct ImpactOutput {
    #[serde(flatten)]
    pub(crate) impact: Impact,
    pub(crate) completeness: ResultBounds,
}

#[derive(Debug, Serialize)]
pub(crate) struct TraceFlowOutput {
    pub(crate) entry_point: NodeId,
    pub(crate) depth_limit: u32,
    pub(crate) step_count: usize,
    pub(crate) completeness: ResultBounds,
    /// Present when the walk was truncated: pass as `offset` to fetch the next page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_offset: Option<u32>,
    /// Table reads/writes performed by traced methods. Always present; consult
    /// `db_effects_complete` before interpreting an empty array as "none".
    pub(crate) db_effects: Vec<DbEffect>,
    pub(crate) db_effects_complete: bool,
    pub(crate) traversal: TraversalStats,
    pub(crate) steps: Vec<FlowHop>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ComplexityHotspotsOutput {
    pub(crate) count: usize,
    pub(crate) completeness: ResultBounds,
    pub(crate) hotspots: Vec<HotspotNode>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FindDuplicatesOutput {
    pub(crate) query_id: NodeId,
    pub(crate) min_jaccard: f32,
    pub(crate) count: usize,
    pub(crate) completeness: ResultBounds,
    pub(crate) similar: Vec<SimilarMethod>,
}

pub(crate) enum SymbolResolution {
    Id(NodeId),
    Ambiguous(Vec<Node>),
    NotFound,
}

pub(crate) async fn resolve_symbol(
    store: &std::sync::Arc<dyn cih_graph_store::GraphStore>,
    name: &str,
) -> Result<SymbolResolution, AppError> {
    if name.contains(':') {
        let id = NodeId::new(name.to_string());
        return repo_node_resolution(store, id).await;
    }
    let candidates = store
        .candidates_by_name(name, 10)
        .await
        .map_err(graph_error)?;
    Ok(match candidates.len() {
        0 => SymbolResolution::NotFound,
        1 => SymbolResolution::Id(candidates.into_iter().next().expect("one candidate").id),
        _ => SymbolResolution::Ambiguous(candidates),
    })
}

async fn repo_node_resolution(
    store: &std::sync::Arc<dyn cih_graph_store::GraphStore>,
    id: NodeId,
) -> Result<SymbolResolution, AppError> {
    Ok(
        if store.get_node(&id).await.map_err(graph_error)?.is_some() {
            SymbolResolution::Id(id)
        } else {
            SymbolResolution::NotFound
        },
    )
}

/// Parse node-kind labels for filtering, rejecting unknown ones loudly — a typo
/// must surface as an error, not silently exclude nothing.
fn parse_node_kinds(labels: &[String]) -> Result<Vec<NodeKind>, AppError> {
    labels
        .iter()
        .map(|label| {
            let kind = NodeKind::from_label(label);
            if kind == NodeKind::Other && label != "Other" {
                return Err(AppError::InvalidInput {
                    field: "exclude_kinds",
                    message: format!("unknown node kind '{label}'"),
                });
            }
            Ok(kind)
        })
        .collect()
}

fn symbol_not_found(name: String) -> AppError {
    AppError::NotFound {
        entity: "symbol",
        key: name,
    }
}

fn graph_error(error: GraphStoreError) -> AppError {
    AppError::from_graph_store(error, "node")
}

fn impact_bounds(impact: &Impact) -> ResultBounds {
    if impact.has_more {
        ResultBounds::backend_limited(impact.affected.len(), impact.backend_limit)
    } else {
        ResultBounds::exact_limit(
            impact.affected.len(),
            impact.affected.len(),
            Some(impact.backend_limit),
        )
    }
}

fn reaches_status(has_paths: bool, traversal_truncated: bool) -> ReachesStatus {
    if has_paths {
        ReachesStatus::Reachable
    } else if traversal_truncated {
        ReachesStatus::Inconclusive
    } else {
        ReachesStatus::NotReachable
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use cih_core::{
        Edge, EdgeKind, GraphArtifacts, GraphDelta, GroupRegistry, Node, NodeId, NodeKind,
        Registry, RegistryEntry, RegistryStats,
    };
    use cih_graph_store::{
        CommunityEdge, CommunityInfo, Direction, FlowNode, FlowPage, GraphOverview, GraphStore,
        GraphSummary, HotspotNode, Impact, ImpactNode, LoadStats, Path as GraphPath,
        Result as StoreResult, RouteInfo, SimilarMethod, Subgraph, SymbolContext,
    };
    use cih_search::SearchHit;

    use super::*;
    use crate::application::app_services::RepoContextService;
    use crate::application::change_detection::ChangeDetectionService;
    use crate::domain::repository::{RepoCatalogSnapshot, RepoSelector, ResolvedRepo};
    use crate::ports::changed_files_source::{ChangeScope, ChangedFilesSource};
    use crate::ports::repo_context_provider::{RepoContext, RepoContextProvider};
    use crate::ports::search_provider::{SearchProvider, SearchProviderError};

    struct DbEffectsFailingStore {
        root: Node,
        routes: Vec<RouteInfo>,
    }

    fn unimplemented<T>() -> StoreResult<T> {
        Err(GraphStoreError::Unimplemented("graph trace test store"))
    }

    #[async_trait]
    impl GraphStore for DbEffectsFailingStore {
        async fn ensure_schema(&self) -> StoreResult<()> {
            Ok(())
        }

        async fn bulk_load(&self, _artifacts: &GraphArtifacts) -> StoreResult<LoadStats> {
            unimplemented()
        }

        async fn upsert_incremental(&self, _delta: &GraphDelta) -> StoreResult<()> {
            unimplemented()
        }

        async fn publish_to(&self, _dest_key: &str) -> StoreResult<()> {
            unimplemented()
        }

        async fn drop_graph(&self) -> StoreResult<()> {
            unimplemented()
        }

        async fn get_node(&self, id: &NodeId) -> StoreResult<Option<Node>> {
            Ok((id == &self.root.id).then(|| self.root.clone()))
        }

        async fn neighbors(
            &self,
            _id: &NodeId,
            _dir: Direction,
            _kinds: &[EdgeKind],
        ) -> StoreResult<Vec<Edge>> {
            unimplemented()
        }

        async fn impact(
            &self,
            _id: &NodeId,
            _dir: Direction,
            _max_depth: u32,
        ) -> StoreResult<Impact> {
            unimplemented()
        }

        async fn call_chain(
            &self,
            _from: &NodeId,
            _to: &NodeId,
            _max_depth: u32,
        ) -> StoreResult<Vec<GraphPath>> {
            unimplemented()
        }

        async fn subgraph(&self, _seeds: &[NodeId], _radius: u32) -> StoreResult<Subgraph> {
            unimplemented()
        }

        async fn graph_summary(&self) -> StoreResult<GraphSummary> {
            unimplemented()
        }

        async fn graph_overview(
            &self,
            _max_nodes: usize,
            _max_edges: usize,
            _kinds: Option<&[String]>,
        ) -> StoreResult<GraphOverview> {
            unimplemented()
        }

        async fn context(&self, _id: &NodeId) -> StoreResult<SymbolContext> {
            unimplemented()
        }

        async fn communities(&self) -> StoreResult<Vec<CommunityInfo>> {
            unimplemented()
        }

        async fn route_map(
            &self,
            _prefix: Option<&str>,
            limit: usize,
        ) -> StoreResult<Vec<RouteInfo>> {
            // Emulate a backend `LIMIT`: return at most `limit` rows so the
            // application-layer over-fetch probe can observe truncation.
            let mut routes = self.routes.clone();
            routes.truncate(limit);
            Ok(routes)
        }

        async fn candidates_by_name(&self, _name: &str, _limit: usize) -> StoreResult<Vec<Node>> {
            unimplemented()
        }

        async fn nodes_in_files(&self, _files: &[String], _limit: usize) -> StoreResult<Vec<Node>> {
            unimplemented()
        }

        async fn processes_for_symbols(&self, _ids: &[NodeId]) -> StoreResult<Vec<String>> {
            unimplemented()
        }

        async fn flow_downstream(
            &self,
            entry: &NodeId,
            _filter: &FlowFilter,
        ) -> StoreResult<FlowPage> {
            assert_eq!(entry, &self.root.id);
            Ok(FlowPage {
                hops: vec![FlowHop {
                    node: FlowNode {
                        id: self.root.id.clone(),
                        kind: self.root.kind,
                        name: self.root.name.clone(),
                        qualified_name: self.root.qualified_name.clone(),
                        file: self.root.file.clone(),
                        depth: 0,
                        parent_id: None,
                        intercepted_by: Vec::new(),
                    },
                    via: None,
                }],
                has_more: false,
                traversal: TraversalStats {
                    visited_nodes: 1,
                    expanded_edges: 0,
                    truncated: false,
                },
            })
        }

        async fn db_effects_for_methods(&self, ids: &[NodeId]) -> StoreResult<Vec<DbEffect>> {
            assert_eq!(ids, std::slice::from_ref(&self.root.id));
            Err(GraphStoreError::Backend(
                "intentional db-effect failure".to_string(),
            ))
        }

        async fn complexity_hotspots(
            &self,
            _min_cyclomatic: Option<u16>,
            _min_cognitive: Option<u16>,
            _min_transitive_loop: Option<u8>,
            _limit: usize,
        ) -> StoreResult<Vec<HotspotNode>> {
            unimplemented()
        }

        async fn similar_methods(
            &self,
            _id: &NodeId,
            _min_jaccard: f32,
            _limit: usize,
        ) -> StoreResult<Vec<SimilarMethod>> {
            unimplemented()
        }

        async fn symbol_communities(
            &self,
            _ids: &[NodeId],
        ) -> StoreResult<Vec<(NodeId, CommunityInfo)>> {
            unimplemented()
        }

        async fn test_coverage(&self, _id: &NodeId) -> StoreResult<Vec<Node>> {
            unimplemented()
        }

        async fn tests_for_files(&self, _files: &[String]) -> StoreResult<Vec<Node>> {
            unimplemented()
        }

        async fn untested_symbols(
            &self,
            _file_prefix: &str,
            _limit: usize,
        ) -> StoreResult<Vec<Node>> {
            unimplemented()
        }

        async fn community_graph(&self) -> StoreResult<Vec<CommunityEdge>> {
            unimplemented()
        }
    }

    struct FixedRepoContext {
        context: Arc<RepoContext>,
        catalog: RepoCatalogSnapshot,
    }

    #[async_trait]
    impl RepoContextProvider for FixedRepoContext {
        fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
            self.catalog.clone()
        }

        fn resolve_repo(&self, _selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
            Ok(self.context.repo.clone())
        }

        async fn resolve(&self, _selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
            Ok(self.context.clone())
        }
    }

    struct EmptySearch;

    #[async_trait]
    impl SearchProvider for EmptySearch {
        async fn query_hits(
            &self,
            _query: &str,
            _limit: usize,
        ) -> Result<Vec<SearchHit>, SearchProviderError> {
            Ok(Vec::new())
        }
    }

    struct EmptyChangedFiles;

    impl ChangedFilesSource for EmptyChangedFiles {
        fn changed_files(
            &self,
            _repo_path: &str,
            _scope: ChangeScope,
            _base_ref: Option<&str>,
        ) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
    }

    fn service_with_store(store: DbEffectsFailingStore) -> GraphQueryService {
        let entry = RegistryEntry {
            repository_id: None,
            name: "trace-test".to_string(),
            path: "/tmp/trace-test".to_string(),
            graph_key: "trace-test".to_string(),
            artifacts_dir: String::new(),
            latest_artifact_version: None,
            published_artifact_version: None,
            published_graph_content_version: None,
            published_epoch: None,
            community_artifacts_dir: None,
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
            last_git_head: None,
            stats: RegistryStats::default(),
        };
        let context = Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(entry.clone()),
            store: Arc::new(store),
            search: Arc::new(EmptySearch),
        });
        let provider = FixedRepoContext {
            context,
            catalog: RepoCatalogSnapshot::for_test(
                entry.graph_key.clone(),
                Registry {
                    entries: vec![entry],
                },
                GroupRegistry::default(),
            ),
        };
        GraphQueryService::new(
            RepoContextService::new(Arc::new(provider)),
            ChangeDetectionService::new(Arc::new(EmptyChangedFiles)),
        )
    }

    fn trace_service(root: Node) -> GraphQueryService {
        service_with_store(DbEffectsFailingStore {
            root,
            routes: Vec::new(),
        })
    }

    fn route_service(routes: Vec<RouteInfo>) -> GraphQueryService {
        let root = Node {
            id: NodeId::new("Method:test.Api#root/0"),
            kind: NodeKind::Method,
            name: "root".to_string(),
            qualified_name: Some("test.Api.root".to_string()),
            file: "src/test/Api.java".to_string(),
            range: Default::default(),
            props: None,
        };
        service_with_store(DbEffectsFailingStore { root, routes })
    }

    fn sample_route(path: &str) -> RouteInfo {
        RouteInfo {
            path: path.to_string(),
            http_method: "GET".to_string(),
            decorator: "@GetMapping".to_string(),
            handler_id: NodeId::new(format!("Method:test.Api#handle{path}/0")),
            handler_name: "handle".to_string(),
            handler_qualified: format!("test.Api.handle{path}"),
        }
    }

    #[tokio::test]
    async fn route_map_reports_complete_when_the_full_set_fits_the_limit() {
        // Exactly `limit` routes exist: the +1 over-fetch finds no extra row, so the
        // page is the whole set and must be reported complete with an exact total —
        // the case the old always-`backend_limited` path wrongly flagged incomplete.
        let service = route_service(vec![
            sample_route("/a"),
            sample_route("/b"),
            sample_route("/c"),
        ]);
        let output = service
            .routes(RouteMapCommand {
                repo: String::new(),
                prefix: None,
                limit: 3,
            })
            .await
            .expect("route_map should succeed");
        assert_eq!(output.routes.len(), 3);
        assert!(output.completeness.complete);
        assert_eq!(output.completeness.total_known, Some(3));
        assert_eq!(output.completeness.omitted, Some(0));
        assert!(output.completeness.reasons.is_empty());
    }

    #[tokio::test]
    async fn route_map_probe_flags_truncation_in_the_serialized_payload() {
        // Four routes available, caller asks for three: the over-fetch proves a
        // fourth exists, so the page is incomplete. The completeness must ride in
        // the serialized object the tool now emits into the primary MCP content
        // block (json_result, not the legacy bare array).
        let service = route_service(vec![
            sample_route("/a"),
            sample_route("/b"),
            sample_route("/c"),
            sample_route("/d"),
        ]);
        let output = service
            .routes(RouteMapCommand {
                repo: String::new(),
                prefix: None,
                limit: 3,
            })
            .await
            .expect("route_map should succeed");
        assert_eq!(output.routes.len(), 3);
        assert!(!output.completeness.complete);
        assert_eq!(output.completeness.total_known, None);
        assert_eq!(output.completeness.limit, Some(3));

        let json = serde_json::to_value(&output).expect("route output should serialize");
        assert_eq!(json["completeness"]["complete"], serde_json::json!(false));
        assert!(json["routes"].is_array());
    }

    #[test]
    fn truncated_empty_path_status_is_inconclusive() {
        assert_eq!(reaches_status(false, true), ReachesStatus::Inconclusive);
        assert_eq!(reaches_status(false, false), ReachesStatus::NotReachable);
        assert_eq!(reaches_status(true, true), ReachesStatus::Reachable);
    }

    #[test]
    fn context_cursors_are_section_and_symbol_bound() {
        let codec = CursorCodec::for_test([0x42; 32], "context-test-v1");
        let repository = context_repository_binding("a", "epoch-1", "graph-1");
        let symbol = NodeId::new("Method:test.Context#root/0");
        let key = ContextCursorKey {
            name: "caller".to_string(),
            id: "Method:test.Context#caller/0".to_string(),
        };
        let encoded = encode_context_cursor("callers", &symbol, 25, &repository, &key, &codec, 100)
            .expect("cursor should encode");
        assert_eq!(
            decode_context_cursor(
                Some(&encoded),
                "callers",
                &symbol,
                25,
                &repository,
                &codec,
                101,
            )
            .unwrap(),
            Some(key)
        );
        let wrong_section = decode_context_cursor(
            Some(&encoded),
            "callees",
            &symbol,
            25,
            &repository,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(wrong_section.to_string().contains("wrong_section"));
        assert!(decode_context_cursor(
            Some(&encoded),
            "callers",
            &NodeId::new("Method:test.Other#root/0"),
            25,
            &repository,
            &codec,
            101,
        )
        .unwrap_err()
        .to_string()
        .contains("wrong_symbol"));
        assert!(decode_context_cursor(
            Some("v1.not-hex"),
            "callers",
            &symbol,
            25,
            &repository,
            &codec,
            101,
        )
        .unwrap_err()
        .to_string()
        .contains("malformed_cursor"));
    }

    fn context_repository_binding(
        id_digit: &str,
        epoch: &str,
        graph_version: &str,
    ) -> RepositoryCursorBinding {
        RepositoryCursorBinding {
            identity: format!("repository-id:{}", id_digit.repeat(64)),
            published_epoch: Some(epoch.to_string()),
            published_graph_content_version: Some(graph_version.to_string()),
        }
    }

    #[test]
    fn context_cursors_reject_tampering_expiry_and_request_drift() {
        let codec = CursorCodec::for_test([0x24; 32], "context-test-v1");
        let repository = context_repository_binding("a", "epoch-1", "graph-1");
        let symbol = NodeId::new("Method:test.Context#root/0");
        let key = ContextCursorKey {
            name: "caller".to_string(),
            id: "Method:test.Context#caller/0".to_string(),
        };
        let encoded =
            encode_context_cursor("callers", &symbol, 25, &repository, &key, &codec, 100).unwrap();

        let mut tampered = encoded.clone().into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).unwrap();
        let tampered_error = decode_context_cursor(
            Some(&tampered),
            "callers",
            &symbol,
            25,
            &repository,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(tampered_error.to_string().contains("cursor_tampered"));

        let expired_error = decode_context_cursor(
            Some(&encoded),
            "callers",
            &symbol,
            25,
            &repository,
            &codec,
            100 + DEFAULT_CURSOR_TTL_SECS + 1,
        )
        .unwrap_err();
        assert!(expired_error.to_string().contains("cursor_expired"));

        let wrong_limit = decode_context_cursor(
            Some(&encoded),
            "callers",
            &symbol,
            50,
            &repository,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(wrong_limit.to_string().contains("wrong_page_bounds"));

        let other_repository = context_repository_binding("b", "epoch-1", "graph-1");
        let wrong_repository = decode_context_cursor(
            Some(&encoded),
            "callers",
            &symbol,
            25,
            &other_repository,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(wrong_repository.to_string().contains("wrong_repository"));

        let changed_generation = context_repository_binding("a", "epoch-2", "graph-2");
        let generation_error = decode_context_cursor(
            Some(&encoded),
            "callers",
            &symbol,
            25,
            &changed_generation,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(generation_error.to_string().contains("generation_changed"));

        let wrong_filter = codec
            .encode_at(
                CONTEXT_CURSOR_OPERATION,
                DEFAULT_CURSOR_TTL_SECS,
                &ContextCursorPayload {
                    section: "callers".to_string(),
                    symbol: symbol.to_string(),
                    limit: 25,
                    filter_hash: "different-filter".to_string(),
                    repository: repository.clone(),
                    key: key.clone(),
                },
                100,
            )
            .unwrap();
        let filter_error = decode_context_cursor(
            Some(&wrong_filter),
            "callers",
            &symbol,
            25,
            &repository,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(filter_error.to_string().contains("wrong_filter"));

        let wrong_operation = codec
            .encode_at(
                "list_repos_page",
                DEFAULT_CURSOR_TTL_SECS,
                &ContextCursorPayload {
                    section: "callers".to_string(),
                    symbol: symbol.to_string(),
                    limit: 25,
                    filter_hash: context_filter_hash(),
                    repository: repository.clone(),
                    key,
                },
                100,
            )
            .unwrap();
        let operation_error = decode_context_cursor(
            Some(&wrong_operation),
            "callers",
            &symbol,
            25,
            &repository,
            &codec,
            101,
        )
        .unwrap_err();
        assert!(operation_error.to_string().contains("wrong_operation"));
    }

    #[test]
    fn context_limits_default_and_reject_oversize_pages() {
        assert_eq!(context_limit("caller_limit", 0).unwrap(), 100);
        assert_eq!(context_limit("caller_limit", 500).unwrap(), 500);
        assert!(context_limit("caller_limit", 501).is_err());
    }

    #[test]
    fn capped_impact_is_never_reported_as_complete() {
        let impact = Impact {
            root: NodeId::new("Method:test.Impact#root/0"),
            direction: Direction::Upstream,
            affected: vec![ImpactNode {
                id: NodeId::new("Method:test.Impact#caller/0"),
                depth: 1,
                via: "CALLS".to_string(),
                name: "caller".to_string(),
                kind: "Method".to_string(),
                parent_id: None,
            }],
            risk: "low".to_string(),
            has_more: true,
            backend_limit: 200,
            traversal: TraversalStats {
                visited_nodes: 2,
                expanded_edges: 0,
                truncated: true,
            },
            risk_exact: false,
            risk_lower_bound: true,
        };
        let bounds = impact_bounds(&impact);
        assert!(!bounds.complete);
        assert_eq!(bounds.total_known, None);
        assert_eq!(bounds.reasons, vec!["backend_limit"]);
    }

    #[tokio::test]
    async fn trace_serializes_explicit_db_effect_failure_completeness() {
        let root = Node {
            id: NodeId::new("Method:test.Trace#run/0"),
            kind: NodeKind::Method,
            name: "run".to_string(),
            qualified_name: Some("test.Trace.run".to_string()),
            file: "src/test/Trace.java".to_string(),
            range: Default::default(),
            props: None,
        };
        let output = trace_service(root.clone())
            .trace_flow(TraceFlowCommand {
                repo: String::new(),
                entry_point: root.id.to_string(),
                max_depth: 3,
                exclude_kinds: Vec::new(),
                business_only: false,
                max_nodes: 20,
                offset: 0,
            })
            .await
            .expect("the trace itself remains available when DB effects fail");
        let SymbolQueryOutput::Resolved(trace) = output else {
            panic!("full node id must resolve without ambiguity");
        };

        assert!(trace.db_effects.is_empty());
        assert!(!trace.db_effects_complete);
        assert!(!trace.completeness.complete);
        assert_eq!(trace.completeness.failed, 1);
        assert_eq!(trace.completeness.reasons, vec!["db_effects_unavailable"]);

        let json = serde_json::to_value(&trace).expect("trace output should serialize");
        assert_eq!(json["db_effects"], serde_json::json!([]));
        assert_eq!(json["db_effects_complete"], serde_json::json!(false));
        assert_eq!(json["completeness"]["failed"], serde_json::json!(1));
        assert_eq!(
            json["completeness"]["reasons"],
            serde_json::json!(["db_effects_unavailable"])
        );
    }
}
