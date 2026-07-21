use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::AppError;
use crate::domain::repository::ResolvedRepo;
use crate::ports::artifact_repository::ArtifactRepository;
use crate::ports::cross_repo_graph_provider::{CrossRepoGraph, CrossRepoGraphProvider};

#[derive(Clone)]
pub(crate) struct ArtifactCrossRepoGraphProvider {
    artifacts: Arc<dyn ArtifactRepository>,
}

impl ArtifactCrossRepoGraphProvider {
    pub(crate) fn new(artifacts: Arc<dyn ArtifactRepository>) -> Self {
        Self { artifacts }
    }
}

#[async_trait]
impl CrossRepoGraphProvider for ArtifactCrossRepoGraphProvider {
    async fn graph_for(&self, repo: &ResolvedRepo) -> Result<Arc<CrossRepoGraph>, AppError> {
        self.artifacts
            .indexed_snapshot(repo)
            .await
            .map(CrossRepoGraph::from_snapshot)
            .map(Arc::new)
    }
}
