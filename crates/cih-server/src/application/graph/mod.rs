//! Repository-scoped graph query use cases.

use cih_core::{Node, NodeId};
use cih_graph_store::{
    CommunityEdge, CommunityInfo, Direction, FlowHop, GraphStoreError, HotspotNode, Impact,
    RouteInfo, SimilarMethod, SymbolContext,
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
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        match resolve_symbol(&repo.store, &command.entry_point).await? {
            SymbolResolution::Id(id) => {
                let steps = repo
                    .store
                    .flow_downstream(&id, command.max_depth)
                    .await
                    .map_err(graph_error)?;
                Ok(SymbolQueryOutput::Resolved(TraceFlowOutput {
                    entry_point: id,
                    depth_limit: command.max_depth,
                    step_count: steps.len(),
                    completeness: ResultBounds::requested_scope(steps.len()),
                    steps,
                }))
            }
            SymbolResolution::Ambiguous(nodes) => Ok(SymbolQueryOutput::Ambiguous(
                AmbiguousResult::from_nodes(nodes),
            )),
            SymbolResolution::NotFound => Err(symbol_not_found(command.entry_point)),
        }
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
        return Ok(SymbolResolution::Id(NodeId::new(name.to_string())));
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
        other => AppError::Unavailable {
            dependency: "graph store",
            message: other.to_string(),
            retryable: true,
        },
    }
}
