use std::fmt;

use async_trait::async_trait;
use cih_search::SearchHit;

#[derive(Clone, Debug)]
pub(crate) struct SearchProviderError {
    message: String,
    retryable: bool,
}

impl SearchProviderError {
    pub(crate) fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for SearchProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SearchProviderError {}

#[async_trait]
pub(crate) trait SearchProvider: Send + Sync {
    async fn query_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, SearchProviderError>;
}
