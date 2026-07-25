//! Repository-scoped graph query use cases.

use cih_core::{Node, NodeId, NodeKind};
use cih_graph_store::{
    CommunityEdge, CommunityInfo, DbEffect, Direction, FlowFilter, FlowHop, GraphStoreError,
    HotspotNode, Impact, PathAccess, PathFilter, RouteInfo, SimilarMethod, SymbolContext,
    TraversalStats, FLOW_VISIBLE_WINDOW,
};
use serde::Serialize;

use crate::application::app_services::RepoContextService;
use crate::application::change_detection::{
    ChangeDetectionService, DetectChangesCommand, DetectChangesOutput,
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
    ) -> Result<SymbolQueryOutput<SymbolContext>, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        match resolve_symbol(&repo.store, &command.name).await? {
            SymbolResolution::Id(id) => repo
                .store
                .context(&id)
                .await
                .map(SymbolQueryOutput::Resolved)
                .map_err(graph_error),
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
                    SymbolQueryOutput::Resolved(ImpactOutput {
                        completeness: ResultBounds::requested_scope(impact.affected.len()),
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
        let routes = repo
            .store
            .route_map(command.prefix.as_deref(), command.limit)
            .await
            .map_err(graph_error)?;
        let completeness = ResultBounds::backend_limited(routes.len(), command.limit);
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
    match error {
        GraphStoreError::NotFound(key) => AppError::NotFound {
            entity: "node",
            key,
        },
        GraphStoreError::InvalidInput(message) => AppError::InvalidInput {
            field: "graph query",
            message,
        },
        other => AppError::Unavailable {
            dependency: "graph store",
            message: other.to_string(),
            retryable: true,
        },
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
        GraphSummary, HotspotNode, Impact, LoadStats, Path as GraphPath, Result as StoreResult,
        RouteInfo, SimilarMethod, Subgraph, SymbolContext,
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
            _limit: usize,
        ) -> StoreResult<Vec<RouteInfo>> {
            unimplemented()
        }

        async fn candidates_by_name(&self, _name: &str, _limit: usize) -> StoreResult<Vec<Node>> {
            unimplemented()
        }

        async fn nodes_in_files(&self, _files: &[String]) -> StoreResult<Vec<Node>> {
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

    fn trace_service(root: Node) -> GraphQueryService {
        let entry = RegistryEntry {
            name: "trace-test".to_string(),
            path: "/tmp/trace-test".to_string(),
            graph_key: "trace-test".to_string(),
            artifacts_dir: String::new(),
            community_artifacts_dir: None,
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
            last_git_head: None,
            stats: RegistryStats::default(),
        };
        let context = Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(entry.clone()),
            store: Arc::new(DbEffectsFailingStore { root }),
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

    #[test]
    fn truncated_empty_path_status_is_inconclusive() {
        assert_eq!(reaches_status(false, true), ReachesStatus::Inconclusive);
        assert_eq!(reaches_status(false, false), ReachesStatus::NotReachable);
        assert_eq!(reaches_status(true, true), ReachesStatus::Reachable);
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
