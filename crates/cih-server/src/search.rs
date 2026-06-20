use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cih_core::{GraphArtifacts, VersionId};
use cih_embed::{EmbedStore, SemanticHit};
use cih_graph_store::Subgraph;
use cih_search::{rrf_merge, SearchHit, SearchIndex};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct QueryArgs {
    /// Natural language or symbol keyword query.
    pub(crate) q: String,
    /// Maximum number of fused hits to return (default 10).
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Include a one-hop subgraph around the top results.
    #[serde(default)]
    pub(crate) expand: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryResult {
    pub(crate) hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subgraph: Option<Subgraph>,
}

#[derive(Clone)]
struct CachedIndex {
    index: SearchIndex,
    version: String,
}

#[derive(Clone)]
pub(crate) struct SearchState {
    bm25: Arc<RwLock<Option<CachedIndex>>>,
    embed_store: Option<Arc<EmbedStore>>,
    artifacts_dir: Option<PathBuf>,
}

impl SearchState {
    pub(crate) fn new(
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
    ) -> Self {
        Self {
            bm25: Arc::new(RwLock::new(None)),
            embed_store,
            artifacts_dir,
        }
    }

    pub(crate) async fn query_hits(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        if self.artifacts_dir.is_none() && self.embed_store.is_none() {
            return Err(anyhow!(
                "query unavailable: set CIH_ARTIFACTS_DIR for BM25 and/or CIH_PG_URL for semantic search"
            ));
        }

        let lexical = if let Some(index) = self.bm25_index().await? {
            index.search(query, limit * 2)
        } else {
            Vec::new()
        };
        let semantic = if let Some(embed_store) = &self.embed_store {
            embed_store
                .semantic_search(query, limit * 2, 0.5)
                .await?
                .into_iter()
                .map(semantic_to_search_hit)
                .collect()
        } else {
            Vec::new()
        };

        Ok(rrf_merge(lexical, semantic, limit))
    }

    async fn bm25_index(&self) -> Result<Option<SearchIndex>> {
        let Some(artifacts_dir) = &self.artifacts_dir else {
            return Ok(None);
        };

        let artifacts = latest_graph_artifacts_in_dir(artifacts_dir)?;
        let latest_version = artifacts.version.0.clone();

        {
            let guard = self.bm25.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.version == latest_version {
                    return Ok(Some(cached.index.clone()));
                }
            }
        }

        let nodes = artifacts.read_nodes()?;
        let index = SearchIndex::build(&nodes);
        let mut guard = self.bm25.write().await;
        *guard = Some(CachedIndex {
            index: index.clone(),
            version: latest_version,
        });
        Ok(Some(index))
    }
}

pub(crate) fn query_limit(raw: Option<usize>) -> usize {
    raw.unwrap_or(10).clamp(1, 50)
}

fn semantic_to_search_hit(hit: SemanticHit) -> SearchHit {
    SearchHit::from_parts(
        hit.node_id,
        hit.kind,
        hit.name,
        None,
        hit.file,
        hit.range,
        hit.score,
        "semantic",
    )
}

pub(crate) fn latest_graph_artifacts_in_dir(parent: &Path) -> Result<GraphArtifacts> {
    let entries = std::fs::read_dir(parent)
        .map_err(|err| anyhow!("no graph artifacts at {}: {err}", parent.display()))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let nodes_path = dir.join("nodes.jsonl");
        let edges_path = dir.join("edges.jsonl");
        if !nodes_path.is_file() || !edges_path.is_file() {
            continue;
        }
        let version = entry.file_name().to_string_lossy().into_owned();
        let modified = std::fs::metadata(&nodes_path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((
            modified,
            GraphArtifacts {
                nodes_path,
                edges_path,
                version: VersionId(version),
            },
        ));
    }
    candidates.sort_by(|(left_mtime, left), (right_mtime, right)| {
        right_mtime
            .cmp(left_mtime)
            .then_with(|| right.version.0.cmp(&left.version.0))
    });
    candidates
        .into_iter()
        .next()
        .map(|(_, artifacts)| artifacts)
        .ok_or_else(|| anyhow!("no complete graph artifacts under {}", parent.display()))
}

#[cfg(test)]
mod tests;

