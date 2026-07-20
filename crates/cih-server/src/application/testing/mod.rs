//! Test coverage, regression scope, and security analysis use cases.

use cih_core::{Node, NodeId};
use serde::Serialize;

use crate::application::app_services::RepoContextService;
use crate::application::graph::{
    resolve_symbol, AmbiguousResult, SymbolQueryOutput, SymbolResolution,
};
use crate::application::taint::{TaintPathsCommand, TaintPathsOutput, TaintService};
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;

#[derive(Clone)]
pub(crate) struct TestingService {
    repos: RepoContextService,
    taint: TaintService,
}

impl TestingService {
    pub(crate) fn new(repos: RepoContextService, taint: TaintService) -> Self {
        Self { repos, taint }
    }

    pub(crate) async fn test_coverage(
        &self,
        command: TestCoverageCommand,
    ) -> Result<SymbolQueryOutput<TestCoverageOutput>, AppError> {
        let repo = self.resolve(&command.repo).await?;
        match resolve_symbol(&repo.store, &command.name).await? {
            SymbolResolution::Id(id) => {
                let tests = repo.store.test_coverage(&id).await.map_err(graph_error)?;
                Ok(SymbolQueryOutput::Resolved(TestCoverageOutput {
                    symbol_id: id,
                    test_count: tests.len(),
                    tests: tests.into_iter().map(TestNodeOutput::from).collect(),
                }))
            }
            SymbolResolution::Ambiguous(nodes) => Ok(SymbolQueryOutput::Ambiguous(
                AmbiguousResult::from_nodes(nodes),
            )),
            SymbolResolution::NotFound => Err(AppError::NotFound {
                entity: "symbol",
                key: command.name,
            }),
        }
    }

    pub(crate) async fn regression_scope(
        &self,
        command: RegressionScopeCommand,
    ) -> Result<RegressionScopeOutput, AppError> {
        let repo = self.resolve(&command.repo).await?;
        let tests = repo
            .store
            .tests_for_files(&command.changed_files)
            .await
            .map_err(graph_error)?;
        let mut seen_files = std::collections::BTreeSet::new();
        let test_classes = tests
            .into_iter()
            .filter(|node| seen_files.insert(node.file.clone()))
            .map(TestNodeOutput::from)
            .collect();
        Ok(RegressionScopeOutput {
            changed_file_count: command.changed_files.len(),
            test_class_count: seen_files.len(),
            test_classes,
        })
    }

    pub(crate) async fn untested_paths(
        &self,
        command: UntestedPathsCommand,
    ) -> Result<UntestedPathsOutput, AppError> {
        let repo = self.resolve(&command.repo).await?;
        let symbols = repo
            .store
            .untested_symbols(&command.module_prefix, command.limit)
            .await
            .map_err(graph_error)?;
        Ok(UntestedPathsOutput {
            prefix: command.module_prefix,
            untested_count: symbols.len(),
            symbols: symbols.into_iter().map(TestNodeOutput::from).collect(),
        })
    }

    pub(crate) async fn taint_paths(
        &self,
        repo: String,
        command: TaintPathsCommand,
    ) -> Result<TaintPathsOutput, AppError> {
        let repo = self.repos.resolve_repo(RepoSelector::from_wire(&repo))?;
        self.taint.taint_paths(repo, command).await
    }

    async fn resolve(&self, repo: &str) -> Result<std::sync::Arc<ResolvedRepoContext>, AppError> {
        self.repos.resolve(RepoSelector::from_wire(repo)).await
    }
}

type ResolvedRepoContext = crate::ports::repo_context_provider::RepoContext;

pub(crate) struct TestCoverageCommand {
    pub(crate) repo: String,
    pub(crate) name: String,
}

pub(crate) struct RegressionScopeCommand {
    pub(crate) repo: String,
    pub(crate) changed_files: Vec<String>,
}

pub(crate) struct UntestedPathsCommand {
    pub(crate) repo: String,
    pub(crate) module_prefix: String,
    pub(crate) limit: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct TestCoverageOutput {
    pub(crate) symbol_id: NodeId,
    pub(crate) test_count: usize,
    pub(crate) tests: Vec<TestNodeOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RegressionScopeOutput {
    pub(crate) changed_file_count: usize,
    pub(crate) test_class_count: usize,
    pub(crate) test_classes: Vec<TestNodeOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct UntestedPathsOutput {
    pub(crate) prefix: String,
    pub(crate) untested_count: usize,
    pub(crate) symbols: Vec<TestNodeOutput>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TestNodeOutput {
    pub(crate) id: NodeId,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) file: String,
}

impl From<Node> for TestNodeOutput {
    fn from(node: Node) -> Self {
        Self {
            id: node.id,
            kind: node.kind.label().to_string(),
            name: node.name,
            file: node.file,
        }
    }
}

fn graph_error(error: cih_graph_store::GraphStoreError) -> AppError {
    AppError::Unavailable {
        dependency: "graph store",
        message: error.to_string(),
        retryable: true,
    }
}
