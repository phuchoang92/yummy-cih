//! Repository-scoped code search use cases.

use std::collections::BTreeMap;

use cih_core::NodeId;
use cih_graph_store::{CommunityInfo, Subgraph};
use cih_search::SearchHit;
use serde::Serialize;

use crate::application::app_services::RepoContextService;
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;

#[derive(Clone)]
pub(crate) struct SearchService {
    repos: RepoContextService,
}

impl SearchService {
    pub(crate) fn new(repos: RepoContextService) -> Self {
        Self { repos }
    }

    pub(crate) async fn query(&self, command: QueryCommand) -> Result<QueryOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let hits = repo
            .search
            .query_hits(&command.query, command.limit)
            .await
            .map_err(search_error)?;
        let subgraph = if command.expand && !hits.is_empty() {
            let seeds: Vec<NodeId> = hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(repo.store.subgraph(&seeds, 1).await.map_err(graph_error)?)
        } else {
            None
        };
        Ok(QueryOutput { hits, subgraph })
    }

    pub(crate) async fn search_code(
        &self,
        command: SearchCodeCommand,
    ) -> Result<Vec<CodeMatchOutput>, AppError> {
        let output = self
            .query(QueryCommand {
                repo: command.repo,
                query: command.query,
                limit: command.limit,
                expand: false,
            })
            .await?;
        Ok(output
            .hits
            .into_iter()
            .map(|hit| CodeMatchOutput {
                node_id: hit.node_id.to_string(),
                kind: hit.kind.label().to_string(),
                name: hit.name,
                qualified_name: hit.qualified_name,
                file: hit.file,
                line: hit.range.start_line,
                score: hit.score,
                rank: hit.rank as u32,
            })
            .collect())
    }

    pub(crate) async fn feature_map(
        &self,
        command: FeatureMapCommand,
    ) -> Result<FeatureMapOutput, AppError> {
        let repo = self
            .repos
            .resolve(RepoSelector::from_wire(&command.repo))
            .await?;
        let hits = repo
            .search
            .query_hits(&command.query, command.limit)
            .await
            .map_err(search_error)?;
        let hit_ids: Vec<NodeId> = hits.iter().map(|hit| hit.node_id.clone()).collect();
        let memberships = repo
            .store
            .symbol_communities(&hit_ids)
            .await
            .map_err(graph_error)?;
        let community_of: BTreeMap<String, CommunityInfo> = memberships
            .into_iter()
            .map(|(node_id, community)| (node_id.to_string(), community))
            .collect();
        let mut grouped: BTreeMap<String, Vec<FeatureMapSymbol>> = BTreeMap::new();
        for hit in &hits {
            let community = community_of
                .get(hit.node_id.as_str())
                .map(|value| value.name.clone())
                .unwrap_or_else(|| "unclustered".to_string());
            grouped
                .entry(community)
                .or_default()
                .push(FeatureMapSymbol {
                    id: hit.node_id.to_string(),
                    kind: hit.kind.label().to_string(),
                    name: hit.name.clone(),
                    file: hit.file.clone(),
                    score: hit.score,
                });
        }
        let clusters = grouped
            .into_iter()
            .map(|(community, symbols)| FeatureMapCluster {
                community,
                symbol_count: symbols.len(),
                symbols,
            })
            .collect();
        Ok(FeatureMapOutput {
            query: command.query,
            total_hits: hits.len(),
            clusters,
        })
    }
}

pub(crate) struct QueryCommand {
    pub(crate) repo: String,
    pub(crate) query: String,
    pub(crate) limit: usize,
    pub(crate) expand: bool,
}

pub(crate) struct SearchCodeCommand {
    pub(crate) repo: String,
    pub(crate) query: String,
    pub(crate) limit: usize,
}

pub(crate) struct FeatureMapCommand {
    pub(crate) repo: String,
    pub(crate) query: String,
    pub(crate) limit: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryOutput {
    pub(crate) hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subgraph: Option<Subgraph>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CodeMatchOutput {
    pub(crate) node_id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) qualified_name: Option<String>,
    pub(crate) file: String,
    pub(crate) line: u32,
    pub(crate) score: f32,
    pub(crate) rank: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeatureMapSymbol {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) file: String,
    pub(crate) score: f32,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeatureMapCluster {
    pub(crate) community: String,
    pub(crate) symbol_count: usize,
    pub(crate) symbols: Vec<FeatureMapSymbol>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FeatureMapOutput {
    pub(crate) query: String,
    pub(crate) total_hits: usize,
    pub(crate) clusters: Vec<FeatureMapCluster>,
}

fn search_error(error: crate::ports::search_provider::SearchProviderError) -> AppError {
    let retryable = error.retryable();
    AppError::Unavailable {
        dependency: "search index",
        message: error.to_string(),
        retryable,
    }
}

fn graph_error(error: cih_graph_store::GraphStoreError) -> AppError {
    AppError::Unavailable {
        dependency: "graph store",
        message: error.to_string(),
        retryable: true,
    }
}
