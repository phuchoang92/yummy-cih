use async_trait::async_trait;
use cih_search::SearchHit;

#[async_trait]
pub(crate) trait SearchProvider: Send + Sync {
    async fn query_hits(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>>;
}
