//! Artifact snapshot contract shared by analysis use cases and local adapters.

use std::collections::HashMap;
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use cih_core::{Edge, Node};

use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;

pub(crate) struct ArtifactIndexes {
    pub(crate) node_by_id: HashMap<String, usize>,
    pub(crate) outgoing_edges: HashMap<String, Vec<usize>>,
    pub(crate) incoming_edges: HashMap<String, Vec<usize>>,
}

impl ArtifactIndexes {
    pub(crate) fn build(nodes: &[Node], edges: &[Edge]) -> Self {
        let node_by_id = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.as_str().to_string(), index))
            .collect();
        let mut outgoing_edges: HashMap<String, Vec<usize>> = HashMap::new();
        let mut incoming_edges: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, edge) in edges.iter().enumerate() {
            outgoing_edges
                .entry(edge.src.as_str().to_string())
                .or_default()
                .push(index);
            incoming_edges
                .entry(edge.dst.as_str().to_string())
                .or_default()
                .push(index);
        }
        Self {
            node_by_id,
            outgoing_edges,
            incoming_edges,
        }
    }
}

pub(crate) struct ArtifactSnapshot {
    pub(crate) version: String,
    pub(crate) nodes: Arc<[Node]>,
    pub(crate) edges: Arc<[Edge]>,
    indexes: OnceLock<Arc<ArtifactIndexes>>,
}

impl ArtifactSnapshot {
    pub(crate) fn from_parts(version: String, nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        Self {
            version,
            nodes: nodes.into(),
            edges: edges.into(),
            indexes: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_memory(nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        let snapshot = Self::from_parts("memory".to_string(), nodes, edges);
        snapshot.ensure_indexes_blocking();
        snapshot
    }

    pub(crate) fn ensure_indexes_blocking(&self) -> &Arc<ArtifactIndexes> {
        self.indexes
            .get_or_init(|| Arc::new(ArtifactIndexes::build(&self.nodes, &self.edges)))
    }

    pub(crate) fn ensure_indexes_with<F>(&self, build: F) -> &Arc<ArtifactIndexes>
    where
        F: FnOnce() -> ArtifactIndexes,
    {
        self.indexes.get_or_init(|| Arc::new(build()))
    }

    pub(crate) fn indexes(&self) -> &Arc<ArtifactIndexes> {
        self.indexes
            .get()
            .expect("indexed snapshot must be prepared by ArtifactRepository")
    }

    pub(crate) fn estimated_weight_bytes(&self) -> usize {
        let base = size_of::<Self>()
            .saturating_add(self.version.capacity())
            .saturating_add(self.nodes.len().saturating_mul(size_of::<Node>()))
            .saturating_add(self.edges.len().saturating_mul(size_of::<Edge>()));
        let node_dynamic = self.nodes.iter().fold(0usize, |total, node| {
            total
                .saturating_add(node.id.as_str().len())
                .saturating_add(node.name.capacity())
                .saturating_add(node.qualified_name.as_ref().map_or(0, String::capacity))
                .saturating_add(node.file.capacity())
                .saturating_add(node.props.as_ref().map_or(0, json_weight))
        });
        let edge_dynamic = self.edges.iter().fold(0usize, |total, edge| {
            total
                .saturating_add(edge.src.as_str().len())
                .saturating_add(edge.dst.as_str().len())
                .saturating_add(edge.reason.capacity())
                .saturating_add(edge.props.as_ref().map_or(0, json_weight))
        });
        let node_index_estimate = self
            .nodes
            .iter()
            .map(|node| {
                node.id
                    .as_str()
                    .len()
                    .saturating_add(size_of::<(String, usize)>().saturating_mul(2))
            })
            .fold(0usize, usize::saturating_add);
        let adjacency_index_estimate = self
            .edges
            .iter()
            .map(|edge| {
                edge.src
                    .as_str()
                    .len()
                    .saturating_add(edge.dst.as_str().len())
                    .saturating_add(size_of::<(String, Vec<usize>)>().saturating_mul(4))
                    .saturating_add(size_of::<usize>().saturating_mul(2))
            })
            .fold(0usize, usize::saturating_add);
        base.saturating_add(node_dynamic)
            .saturating_add(edge_dynamic)
            .saturating_add(node_index_estimate)
            .saturating_add(adjacency_index_estimate)
    }

    #[cfg(test)]
    pub(crate) fn indexes_initialized(&self) -> bool {
        self.indexes.get().is_some()
    }
}

fn json_weight(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            size_of::<serde_json::Value>()
        }
        serde_json::Value::String(value) => {
            size_of::<serde_json::Value>().saturating_add(value.capacity())
        }
        serde_json::Value::Array(values) => values.iter().fold(
            size_of::<serde_json::Value>().saturating_add(
                values
                    .capacity()
                    .saturating_mul(size_of::<serde_json::Value>()),
            ),
            |total, value| total.saturating_add(json_weight(value)),
        ),
        serde_json::Value::Object(values) => {
            values
                .iter()
                .fold(size_of::<serde_json::Value>(), |total, (key, value)| {
                    total
                        .saturating_add(key.capacity())
                        .saturating_add(json_weight(value))
                })
        }
    }
}

#[async_trait]
pub(crate) trait ArtifactRepository: Send + Sync {
    async fn snapshot(&self, repo: &ResolvedRepo) -> Result<Arc<ArtifactSnapshot>, AppError>;

    async fn indexed_snapshot(
        &self,
        repo: &ResolvedRepo,
    ) -> Result<Arc<ArtifactSnapshot>, AppError>;

    fn invalidate_repo(&self, repo_path: &Path) -> usize;
}
