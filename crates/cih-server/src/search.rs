use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cih_embed::{EmbedStore, SemanticHit};
use cih_graph_store::Subgraph;
use cih_search::{rrf_merge, SearchHit, SearchIndex};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::blocking::{blocking_timeout, run_blocking};

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

#[derive(Clone)]
pub struct SearchState {
    bm25: Arc<RwLock<Option<CachedIndex>>>,
    /// Single-flight gate: the BM25 index is a single slot, so one gate
    /// serializes its (expensive) rebuild across concurrent misses.
    bm25_gate: Arc<tokio::sync::Mutex<()>>,
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
            bm25_gate: Arc::new(tokio::sync::Mutex::new(())),
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

        if let Some(hit) = self.peek_bm25(&latest_version).await {
            return Ok(Some(hit));
        }

        // Single-flight: only one build runs at a time; concurrent callers wait
        // on the gate and re-check, reusing the fresh build instead of each
        // rebuilding the (CPU-heavy) index.
        let _held = self.bm25_gate.lock().await;
        if let Some(hit) = self.peek_bm25(&latest_version).await {
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
        let mut guard = self.bm25.write().await;
        *guard = Some(CachedIndex {
            index: index.clone(),
            version: latest_version,
        });
        Ok(Some(index))
    }

    /// The cached BM25 index iff it matches `version` (a cheap Arc clone — the
    /// index's docs + postings are shared, not copied, on every query).
    async fn peek_bm25(&self, version: &str) -> Option<Arc<SearchIndex>> {
        let guard = self.bm25.read().await;
        let cached = guard.as_ref()?;
        (cached.version == version).then(|| cached.index.clone())
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
