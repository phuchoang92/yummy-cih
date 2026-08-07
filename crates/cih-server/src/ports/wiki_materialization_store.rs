//! Stable page-level read boundary for generated or resident wiki content.

use async_trait::async_trait;

use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MaterializedWikiPage {
    pub(crate) slug: String,
    pub(crate) version: String,
    pub(crate) content: String,
}

#[async_trait]
pub(crate) trait WikiMaterializationStore: Send + Sync {
    async fn get_page(
        &self,
        repo: &ResolvedRepo,
        slug: &str,
    ) -> Result<MaterializedWikiPage, AppError>;
}
