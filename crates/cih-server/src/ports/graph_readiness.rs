//! Backend-neutral readiness probe consumed by the application monitor.

use async_trait::async_trait;
use cih_graph_store::{BackendReadiness, Result};

#[async_trait]
pub(crate) trait GraphReadiness: Send + Sync {
    async fn backend_readiness(&self) -> Result<BackendReadiness>;
}
