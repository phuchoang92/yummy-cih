//! Application service registry shared by MCP and HTTP transports.

use std::sync::Arc;

use crate::application::admin::resolve_patterns::ResolvePatternService;
use crate::application::admin::RepositoryAdminService;
use crate::application::architecture_overview::ArchitectureOverviewService;
use crate::application::browser::GraphBrowserService;
use crate::application::contracts::ContractService;
use crate::application::files::FileService;
use crate::application::graph::GraphQueryService;
use crate::application::indexing::IndexingService;
use crate::application::search::SearchService;
use crate::application::testing::TestingService;
use crate::application::wiki_search::{WikiPageService, WikiSearchService};
use crate::domain::error::AppError;
use crate::domain::repository::{RepoCatalogSnapshot, RepoSelector, ResolvedRepo};
use crate::ports::repo_context_provider::{RepoContext, RepoContextProvider};

#[derive(Clone)]
pub(crate) struct RepoContextService {
    provider: Arc<dyn RepoContextProvider>,
}

impl RepoContextService {
    pub(crate) fn new(provider: Arc<dyn RepoContextProvider>) -> Self {
        Self { provider }
    }

    pub(crate) fn resolve_repo(&self, selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
        self.provider.resolve_repo(selector)
    }

    pub(crate) async fn resolve(
        &self,
        selector: RepoSelector,
    ) -> Result<Arc<RepoContext>, AppError> {
        self.provider.resolve(selector).await
    }

    pub(crate) fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
        self.provider.catalog_snapshot()
    }
}

#[derive(Clone)]
pub(crate) struct GraphUseCases {
    pub(crate) queries: GraphQueryService,
    pub(crate) architecture_overview: ArchitectureOverviewService,
    pub(crate) browser: GraphBrowserService,
}

#[derive(Clone)]
pub(crate) struct SearchUseCases {
    pub(crate) queries: SearchService,
}

#[derive(Clone)]
pub(crate) struct CrossRepoUseCases {
    pub(crate) contracts: ContractService,
}

#[derive(Clone)]
pub(crate) struct TestingUseCases {
    pub(crate) analysis: TestingService,
}

#[derive(Clone)]
pub(crate) struct DocsUseCases {
    pub(crate) wiki_search: WikiSearchService,
    pub(crate) wiki_page: WikiPageService,
}

#[derive(Clone)]
pub(crate) struct FileUseCases {
    pub(crate) access: FileService,
}

#[derive(Clone)]
pub(crate) struct AdminUseCases {
    pub(crate) repositories: RepositoryAdminService,
    pub(crate) indexing: IndexingService,
    pub(crate) patterns: ResolvePatternService,
}

#[derive(Clone)]
pub(crate) struct AppServices {
    pub(crate) repos: RepoContextService,
    pub(crate) graph: GraphUseCases,
    pub(crate) search: SearchUseCases,
    pub(crate) cross_repo: CrossRepoUseCases,
    pub(crate) testing: TestingUseCases,
    pub(crate) docs: DocsUseCases,
    pub(crate) files: FileUseCases,
    pub(crate) admin: AdminUseCases,
}
