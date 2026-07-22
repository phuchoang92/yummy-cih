use async_trait::async_trait;

use crate::application::files::grep_runtime_metrics;
use crate::infrastructure::cache::weighted::AsyncCacheMetrics;
use crate::infrastructure::search_provider::SearchCache;
use crate::infrastructure::wiki_repository::{wiki_runtime_metrics, WikiSearchState};
use crate::ports::retrieval_metrics::{
    CacheMetricsSnapshot, RetrievalMetricsProvider, RetrievalMetricsSnapshot,
};

pub(crate) struct RuntimeRetrievalMetrics {
    search: SearchCache,
    wiki: WikiSearchState,
}

impl RuntimeRetrievalMetrics {
    pub(crate) fn new(search: SearchCache, wiki: WikiSearchState) -> Self {
        Self { search, wiki }
    }
}

fn cache_snapshot(metrics: AsyncCacheMetrics) -> CacheMetricsSnapshot {
    CacheMetricsSnapshot {
        requests: metrics.requests,
        hits: metrics.hits,
        misses: metrics.misses,
        builds: metrics.builds,
        retained_entries: metrics.retained_entries,
        retained_weight_bytes: metrics.retained_weight_bytes,
        evictions: metrics.evictions,
        oversize: metrics.oversize,
    }
}

#[async_trait]
impl RetrievalMetricsProvider for RuntimeRetrievalMetrics {
    async fn snapshot(&self) -> RetrievalMetricsSnapshot {
        let (search_cache, wiki_cache) = tokio::join!(self.search.metrics(), self.wiki.metrics());
        RetrievalMetricsSnapshot {
            search_cache: cache_snapshot(search_cache),
            search_runtime: self.search.runtime_metrics(),
            search_indexes: self.search.index_metrics(),
            wiki_cache: cache_snapshot(wiki_cache),
            wiki_runtime: wiki_runtime_metrics(),
            grep: grep_runtime_metrics(),
        }
    }
}
