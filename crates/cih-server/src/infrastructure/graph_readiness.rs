//! Graph-store readiness adapter.

use std::sync::Arc;

use async_trait::async_trait;
use cih_graph_store::{BackendReadiness, GraphStore, Result};

use crate::ports::graph_readiness::GraphReadiness;

pub(crate) struct GraphStoreReadiness {
    store: Arc<dyn GraphStore>,
}

impl GraphStoreReadiness {
    pub(crate) fn new(store: Arc<dyn GraphStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl GraphReadiness for GraphStoreReadiness {
    async fn backend_readiness(&self) -> Result<BackendReadiness> {
        self.store.backend_readiness().await
    }
}
