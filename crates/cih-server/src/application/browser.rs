//! Typed query services used by the graph-browser and readiness HTTP adapters.

use std::path::PathBuf;
use std::sync::Arc;

use cih_core::{Node, NodeId};
use cih_graph_store::{
    CommunityEdge, CommunityInfo, Direction, FlowHop, GraphOverview, GraphStore, GraphStoreError,
    GraphSummary, Impact, RouteInfo, Subgraph, SymbolContext,
};
use cih_search::SearchHit;
use serde::Serialize;

use crate::app_error::AppError;
use crate::search::SearchState;

#[derive(Debug, Serialize)]
pub(crate) struct BrowserSearchResult {
    pub(crate) hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subgraph: Option<Subgraph>,
}

pub(crate) struct BrowserFlow {
    pub(crate) entry_node: Option<Node>,
    pub(crate) hops: Vec<FlowHop>,
}

pub(crate) struct BrowserCommunities {
    pub(crate) communities: Vec<CommunityInfo>,
    pub(crate) edges: Vec<CommunityEdge>,
}

#[derive(Clone)]
pub(crate) struct GraphBrowserService {
    store: Arc<dyn GraphStore>,
    search: SearchState,
}

impl GraphBrowserService {
    pub(crate) fn new(store: Arc<dyn GraphStore>, search: SearchState) -> Self {
        Self { store, search }
    }

    pub(crate) async fn summary(&self) -> Result<GraphSummary, AppError> {
        self.store.graph_summary().await.map_err(graph_error)
    }

    pub(crate) async fn overview(
        &self,
        max_nodes: usize,
        max_edges: usize,
        kinds: Option<&[String]>,
    ) -> Result<GraphOverview, AppError> {
        self.store
            .graph_overview(max_nodes, max_edges, kinds)
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        limit: usize,
        expand: bool,
    ) -> Result<BrowserSearchResult, AppError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(AppError::InvalidInput {
                field: "q",
                message: "query parameter is required".into(),
            });
        }
        let hits = self
            .search
            .query_hits(query, limit)
            .await
            .map_err(|error| AppError::Unavailable {
                dependency: "search index",
                message: error.to_string(),
                retryable: false,
            })?;
        let subgraph = if expand && !hits.is_empty() {
            let seeds: Vec<NodeId> = hits.iter().take(5).map(|hit| hit.node_id.clone()).collect();
            Some(self.store.subgraph(&seeds, 1).await.map_err(graph_error)?)
        } else {
            None
        };
        Ok(BrowserSearchResult { hits, subgraph })
    }

    pub(crate) async fn context(&self, id: &NodeId) -> Result<SymbolContext, AppError> {
        self.store.context(id).await.map_err(graph_error)
    }

    pub(crate) async fn impact(
        &self,
        id: &NodeId,
        direction: Direction,
        depth: u32,
    ) -> Result<Impact, AppError> {
        self.store
            .impact(id, direction, depth)
            .await
            .map_err(graph_error)
    }

    pub(crate) async fn flow(
        &self,
        entry_id: &NodeId,
        depth: u32,
    ) -> Result<BrowserFlow, AppError> {
        let hops = self
            .store
            .flow_downstream(entry_id, depth)
            .await
            .map_err(graph_error)?;
        let entry_node = self.store.get_node(entry_id).await.map_err(graph_error)?;
        Ok(BrowserFlow { entry_node, hops })
    }

    pub(crate) async fn communities(&self) -> Result<BrowserCommunities, AppError> {
        let communities = self.store.communities().await.map_err(graph_error)?;
        let edges = self.store.community_graph().await.map_err(graph_error)?;
        Ok(BrowserCommunities { communities, edges })
    }

    pub(crate) async fn routes(
        &self,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RouteInfo>, AppError> {
        self.store
            .route_map(prefix, limit)
            .await
            .map_err(graph_error)
    }
}

#[derive(Clone)]
pub(crate) struct ReadinessService {
    store: Arc<dyn GraphStore>,
    artifacts_dir: Option<PathBuf>,
}

impl ReadinessService {
    pub(crate) fn new(store: Arc<dyn GraphStore>, artifacts_dir: Option<PathBuf>) -> Self {
        Self {
            store,
            artifacts_dir,
        }
    }

    pub(crate) async fn check(&self) -> ReadinessReport {
        let mut issues = Vec::new();
        if self.store.communities().await.is_err() {
            issues.push("graph store unreachable");
        }
        if self
            .artifacts_dir
            .as_ref()
            .is_some_and(|directory| !directory.exists())
        {
            issues.push("artifacts dir not found");
        }
        ReadinessReport { issues }
    }
}

pub(crate) struct ReadinessReport {
    pub(crate) issues: Vec<&'static str>,
}

impl ReadinessReport {
    pub(crate) fn is_ready(&self) -> bool {
        self.issues.is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_errors_map_to_transport_independent_variants() {
        assert!(matches!(
            graph_error(GraphStoreError::NotFound("Method:x".into())),
            AppError::NotFound {
                entity: "node",
                key
            } if key == "Method:x"
        ));
        assert!(matches!(
            graph_error(GraphStoreError::Backend("down".into())),
            AppError::Unavailable {
                dependency: "graph store",
                retryable: true,
                ..
            }
        ));
    }
}
