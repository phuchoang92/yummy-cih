//! Typed wiki-search application service used by the HTTP adapter.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::domain::error::AppError;
use crate::domain::repository::{RepoSelector, ResolvedRepo};
use crate::ports::repo_context_provider::RepoContextProvider;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WikiSearchCommand {
    query: String,
    repo: RepoSelector,
    facets: WikiSearchFacets,
    limit: usize,
}

impl WikiSearchCommand {
    pub(crate) fn try_new(
        query: String,
        repo: String,
        role: Option<String>,
        kind: Option<String>,
        feature: Option<String>,
        limit: Option<usize>,
    ) -> Result<Self, AppError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(AppError::InvalidInput {
                field: "q",
                message: "query parameter is required".into(),
            });
        }
        Ok(Self {
            query: query.to_string(),
            repo: RepoSelector::from_wire(&repo),
            facets: WikiSearchFacets {
                role: normalized_filter(role),
                kind: normalized_filter(kind),
                feature: normalized_filter(feature),
            },
            limit: limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }
}

fn normalized_filter(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WikiSearchFacets {
    pub(crate) role: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) feature: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WikiSearchHit {
    pub(crate) slug: String,
    pub(crate) role: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) community_id: Option<String>,
    pub(crate) score: f32,
    pub(crate) snippet: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WikiSearchDocument {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) graph_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generated_at: Option<String>,
    pub(crate) query: String,
    pub(crate) page_count: usize,
    pub(crate) hits: Vec<WikiSearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<&'static str>,
}

#[async_trait]
pub(crate) trait WikiSearchRepository: Send + Sync {
    async fn search(
        &self,
        repo: &ResolvedRepo,
        query: &str,
        facets: &WikiSearchFacets,
        limit: usize,
    ) -> Result<WikiSearchDocument, AppError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WikiPageCommand {
    repo: RepoSelector,
    slug: String,
}

impl WikiPageCommand {
    pub(crate) fn try_new(repo: String, slug: String) -> Result<Self, AppError> {
        let slug = slug.trim();
        if slug.is_empty() {
            return Err(AppError::InvalidInput {
                field: "slug",
                message: "page slug is required".into(),
            });
        }
        Ok(Self {
            repo: RepoSelector::from_wire(&repo),
            slug: slug.to_string(),
        })
    }
}

#[async_trait]
pub(crate) trait WikiPageRepository: Send + Sync {
    async fn get_page(&self, repo: &ResolvedRepo, slug: &str) -> Result<String, AppError>;
}

#[derive(Clone)]
pub(crate) struct WikiPageService {
    repo_contexts: Arc<dyn RepoContextProvider>,
    repository: Arc<dyn WikiPageRepository>,
}

impl WikiPageService {
    pub(crate) fn new(
        repo_contexts: Arc<dyn RepoContextProvider>,
        repository: Arc<dyn WikiPageRepository>,
    ) -> Self {
        Self {
            repo_contexts,
            repository,
        }
    }

    pub(crate) async fn get(&self, command: WikiPageCommand) -> Result<String, AppError> {
        let repo = self.repo_contexts.resolve_repo(command.repo)?;
        self.repository.get_page(&repo, &command.slug).await
    }
}

#[derive(Clone)]
pub(crate) struct WikiSearchService {
    repo_contexts: Arc<dyn RepoContextProvider>,
    repository: Arc<dyn WikiSearchRepository>,
}

impl WikiSearchService {
    pub(crate) fn new(
        repo_contexts: Arc<dyn RepoContextProvider>,
        repository: Arc<dyn WikiSearchRepository>,
    ) -> Self {
        Self {
            repo_contexts,
            repository,
        }
    }

    pub(crate) async fn search(
        &self,
        command: WikiSearchCommand,
    ) -> Result<WikiSearchDocument, AppError> {
        let repo = self.repo_contexts.resolve_repo(command.repo)?;
        self.repository
            .search(&repo, &command.query, &command.facets, command.limit)
            .await
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::domain::repository::RepoCatalogSnapshot;
    use crate::ports::repo_context_provider::RepoContext;

    struct FixedRepoContexts {
        repo: ResolvedRepo,
    }

    #[async_trait]
    impl RepoContextProvider for FixedRepoContexts {
        fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
            panic!("wiki search does not request a catalog snapshot")
        }

        fn resolve_repo(&self, _selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
            Ok(self.repo.clone())
        }

        async fn resolve(&self, _selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
            panic!("wiki search does not initialize graph infrastructure")
        }
    }

    struct FixedWikiRepository;

    #[async_trait]
    impl WikiSearchRepository for FixedWikiRepository {
        async fn search(
            &self,
            repo: &ResolvedRepo,
            query: &str,
            facets: &WikiSearchFacets,
            limit: usize,
        ) -> Result<WikiSearchDocument, AppError> {
            assert_eq!(repo.registry_entry.name, "demo");
            assert_eq!(query, "loan");
            assert_eq!(facets.kind.as_deref(), Some("dev"));
            assert_eq!(limit, MAX_LIMIT);
            Ok(WikiSearchDocument {
                repo: Some("demo".into()),
                graph_version: Some("v1".into()),
                generated_at: Some("now".into()),
                query: query.into(),
                page_count: 1,
                hits: Vec::new(),
                source: None,
            })
        }
    }

    #[test]
    fn command_validates_normalizes_and_caps() {
        let command = WikiSearchCommand::try_new(
            " loan ".into(),
            String::new(),
            Some(" ".into()),
            Some(" dev ".into()),
            None,
            Some(500),
        )
        .unwrap();
        assert_eq!(command.query, "loan");
        assert_eq!(command.facets.role, None);
        assert_eq!(command.facets.kind.as_deref(), Some("dev"));
        assert_eq!(command.limit, MAX_LIMIT);
        assert!(
            WikiSearchCommand::try_new(" ".into(), String::new(), None, None, None, None).is_err()
        );
    }

    #[tokio::test]
    async fn service_resolves_repo_and_calls_repository_port() {
        let repo = ResolvedRepo::from_entry(cih_core::RegistryEntry {
            name: "demo".into(),
            path: "/repos/demo".into(),
            graph_key: "demo".into(),
            artifacts_dir: String::new(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        });
        let service = WikiSearchService::new(
            Arc::new(FixedRepoContexts { repo }),
            Arc::new(FixedWikiRepository),
        );
        let output = service
            .search(
                WikiSearchCommand::try_new(
                    "loan".into(),
                    String::new(),
                    None,
                    Some("dev".into()),
                    None,
                    Some(500),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(output.repo.as_deref(), Some("demo"));
        assert_eq!(output.graph_version.as_deref(), Some("v1"));
    }
}
