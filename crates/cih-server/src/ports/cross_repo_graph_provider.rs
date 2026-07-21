//! Repository graph projection used by bounded cross-service traversals.

use std::sync::Arc;

use async_trait::async_trait;
use cih_core::{Edge, Node};

use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;
use crate::ports::artifact_repository::{ArtifactIndexes, ArtifactSnapshot};

pub(crate) struct CrossRepoGraph {
    snapshot: Arc<ArtifactSnapshot>,
    indexes: Arc<ArtifactIndexes>,
}

impl CrossRepoGraph {
    pub(crate) fn from_snapshot(snapshot: Arc<ArtifactSnapshot>) -> Self {
        let indexes = snapshot.indexes().clone();
        Self { snapshot, indexes }
    }

    #[cfg(test)]
    pub(crate) fn build(
        nodes: Vec<Node>,
        edges: Vec<Edge>,
        _nodes_mtime: Option<std::time::SystemTime>,
        _edges_mtime: Option<std::time::SystemTime>,
    ) -> Self {
        Self::from_snapshot(Arc::new(ArtifactSnapshot::from_memory(nodes, edges)))
    }

    pub(crate) fn node(&self, id: &str) -> Option<&Node> {
        self.indexes
            .node_by_id
            .get(id)
            .map(|index| &self.snapshot.nodes[*index])
    }

    pub(crate) fn contains_node(&self, id: &str) -> bool {
        self.indexes.node_by_id.contains_key(id)
    }

    pub(crate) fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.snapshot.nodes.iter()
    }

    pub(crate) fn out<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a Edge> {
        self.indexes
            .outgoing_edges
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.snapshot.edges[*index])
    }

    pub(crate) fn incoming<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a Edge> {
        self.indexes
            .incoming_edges
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.snapshot.edges[*index])
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> &Arc<ArtifactSnapshot> {
        &self.snapshot
    }
}

#[async_trait]
pub(crate) trait CrossRepoGraphProvider: Send + Sync {
    async fn graph_for(&self, repo: &ResolvedRepo) -> Result<Arc<CrossRepoGraph>, AppError>;
}
