use std::sync::Arc;

use async_trait::async_trait;
use cih_graph_store::GraphStore;

use crate::domain::error::AppError;
use crate::domain::repository::{RepoCatalogSnapshot, RepoSelector, ResolvedRepo};
use crate::ports::search_provider::SearchProvider;

#[derive(Clone)]
pub(crate) struct RepoContext {
    pub(crate) repo: ResolvedRepo,
    pub(crate) store: Arc<dyn GraphStore>,
    pub(crate) search: Arc<dyn SearchProvider>,
}

#[async_trait]
pub(crate) trait RepoContextProvider: Send + Sync {
    fn catalog_snapshot(&self) -> RepoCatalogSnapshot;

    fn resolve_repo(&self, selector: RepoSelector) -> Result<ResolvedRepo, AppError>;

    async fn resolve(&self, selector: RepoSelector) -> Result<Arc<RepoContext>, AppError>;
}
