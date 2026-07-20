use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cih_embed::{EmbedStore, SemanticHit};
use cih_graph_store::Subgraph;
use cih_search::{rrf_merge, SearchHit, SearchIndex};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::blocking::{blocking_timeout, run_blocking};
use crate::weighted_cache::{AsyncCacheMetrics, AsyncWeightedCache};

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct QueryArgs {
    /// Natural language or symbol keyword query.
    pub(crate) q: String,
    /// Maximum number of fused hits to return (default 10, pass 0 for default).
    #[serde(default)]
    pub(crate) limit: usize,
    /// Include a one-hop subgraph around the top results.
    #[serde(default)]
    pub(crate) expand: bool,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub(crate) repo: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueryResult {
    pub(crate) hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subgraph: Option<Subgraph>,
}

#[derive(Clone)]
struct CachedIndex {
    index: Arc<SearchIndex>,
    version: String,
}

type SearchGates = Arc<std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>;

#[derive(Clone)]
pub(crate) struct SearchCache {
    indexes: Arc<AsyncWeightedCache<PathBuf, CachedIndex>>,
    gates: SearchGates,
}

impl SearchCache {
    pub(crate) fn new(max_entries: usize, max_weight_bytes: usize) -> Self {
        Self {
            indexes: Arc::new(AsyncWeightedCache::new(max_entries, max_weight_bytes)),
            gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn from_env() -> Self {
        Self::new(
            cache_env("CIH_SEARCH_CACHE_MAX_ENTRIES", 32),
            cache_env(
                "CIH_SEARCH_CACHE_MAX_BYTES",
                crate::config::DEFAULT_SEARCH_CACHE_MAX_BYTES,
            ),
        )
    }

    async fn get(&self, key: &Path, version: &str) -> Option<Arc<SearchIndex>> {
        self.indexes
            .get_if(&key.to_path_buf(), |entry| entry.version == version)
            .await
            .map(|entry| entry.index.clone())
    }

    async fn insert(&self, key: PathBuf, version: String, index: Arc<SearchIndex>) {
        let weight = index.estimated_size_bytes();
        let result = self
            .indexes
            .insert(key, Arc::new(CachedIndex { index, version }), weight)
            .await;
        let retained = result.retained;
        if !result.removed_keys.is_empty() {
            let mut gates = self.gates.lock().unwrap_or_else(|error| error.into_inner());
            for key in result.removed_keys {
                gates.remove(&key);
            }
        }
        let metrics = self.metrics().await;
        tracing::debug!(
            retained,
            weight_bytes = weight,
            cache_hits = metrics.hits,
            cache_misses = metrics.misses,
            cache_builds = metrics.builds,
            cache_entries = metrics.retained_entries,
            cache_weight_bytes = metrics.retained_weight_bytes,
            cache_evictions = metrics.evictions,
            cache_oversize = metrics.oversize,
            "search cache updated"
        );
    }

    fn gate_for(&self, key: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key.to_path_buf())
            .or_default()
            .clone()
    }

    pub(crate) async fn metrics(&self) -> AsyncCacheMetrics {
        self.indexes.metrics().await
    }
}

fn cache_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[derive(Clone)]
pub struct SearchState {
    cache: SearchCache,
    embed_store: Option<Arc<EmbedStore>>,
    artifacts_dir: Option<PathBuf>,
}

impl SearchState {
    #[cfg(test)]
    pub(crate) fn new(
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
    ) -> Self {
        Self::with_cache(artifacts_dir, embed_store, SearchCache::from_env())
    }

    pub(crate) fn with_cache(
        artifacts_dir: Option<PathBuf>,
        embed_store: Option<Arc<EmbedStore>>,
        cache: SearchCache,
    ) -> Self {
        Self {
            cache,
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

    async fn bm25_index(&self) -> Result<Option<Arc<SearchIndex>>> {
        let Some(artifacts_dir) = &self.artifacts_dir else {
            return Ok(None);
        };

        let artifacts = cih_core::GraphArtifacts::latest_in_dir(artifacts_dir)?;
        let latest_version = artifacts.version.to_string();
        let cache_key = artifacts_dir
            .canonicalize()
            .unwrap_or_else(|_| artifacts_dir.clone());

        if let Some(hit) = self.cache.get(&cache_key, &latest_version).await {
            return Ok(Some(hit));
        }

        // Single-flight: only one build runs at a time; concurrent callers wait
        // on the gate and re-check, reusing the fresh build instead of each
        // rebuilding the (CPU-heavy) index.
        let gate = self.cache.gate_for(&cache_key);
        let _held = gate.lock().await;
        if let Some(hit) = self.cache.get(&cache_key, &latest_version).await {
            return Ok(Some(hit));
        }

        // Read + build off the async runtime: on a large repo (~87k nodes) this is
        // CPU-heavy and would otherwise stall a tokio worker thread. No cache lock
        // is held across the blocking call (only the single-flight gate).
        let index = run_blocking(
            blocking_timeout(),
            "bm25 index build",
            move || -> Result<Arc<SearchIndex>> {
                let nodes = artifacts.read_nodes()?;
                Ok(Arc::new(SearchIndex::build(&nodes)))
            },
        )
        .await??;
        self.cache
            .insert(cache_key, latest_version, index.clone())
            .await;
        Ok(Some(index))
    }
}

#[doc(hidden)]
pub fn query_limit(raw: usize) -> usize {
    if raw == 0 { 10 } else { raw }.clamp(1, 50)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repository_states_share_one_weighted_search_cache() {
        let cache = SearchCache::new(8, 1024 * 1024);
        let state_a = SearchState::with_cache(None, None, cache.clone());
        let state_b = SearchState::with_cache(None, None, cache.clone());
        let key = PathBuf::from("/repo/.cih/artifacts");
        let index = Arc::new(SearchIndex::default());
        cache.insert(key.clone(), "v1".into(), index.clone()).await;

        let from_b = state_b.cache.get(&key, "v1").await.unwrap();
        assert!(Arc::ptr_eq(&index, &from_b));
        assert!(Arc::ptr_eq(
            &state_a.cache.get(&key, "v1").await.unwrap(),
            &from_b
        ));
        assert_eq!(cache.metrics().await.retained_entries, 1);
    }

    #[tokio::test]
    async fn oversize_search_index_is_served_but_not_retained() {
        let cache = SearchCache::new(8, 1);
        let key = PathBuf::from("/repo/.cih/artifacts");
        cache
            .insert(key.clone(), "v1".into(), Arc::new(SearchIndex::default()))
            .await;
        assert!(cache.get(&key, "v1").await.is_none());
        let metrics = cache.metrics().await;
        assert_eq!(metrics.retained_entries, 0);
        assert_eq!(metrics.oversize, 1);
    }
}
