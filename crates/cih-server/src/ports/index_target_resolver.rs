use async_trait::async_trait;

use crate::domain::error::AppError;
use crate::domain::indexing::ResolvedRepoTarget;

#[async_trait]
pub(crate) trait IndexTargetResolver: Send + Sync {
    async fn resolve(
        &self,
        repo_path: &str,
        requested_graph_key: &str,
    ) -> Result<ResolvedRepoTarget, AppError>;
}
