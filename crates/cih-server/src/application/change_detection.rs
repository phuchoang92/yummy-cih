//! Typed change-impact application service.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use cih_core::Node;
use cih_graph_store::Direction;
use serde::Serialize;

use crate::domain::completeness::Completeness;
use crate::domain::error::AppError;
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking, BlockingError};
use crate::ports::changed_files_source::{ChangeScope, ChangedFilesSource};
use crate::ports::repo_context_provider::RepoContext;

#[derive(Clone, Debug)]
pub(crate) struct DetectChangesCommand {
    scope: ChangeScope,
    base_ref: Option<String>,
}

impl DetectChangesCommand {
    pub(crate) fn try_new(scope: ChangeScope, base_ref: String) -> Result<Self, AppError> {
        let base_ref = base_ref.trim();
        let base_ref = match scope {
            ChangeScope::BaseRef if base_ref.is_empty() => {
                return Err(AppError::InvalidInput {
                    field: "base_ref",
                    message: "`base_ref` scope requires the `base_ref` argument".into(),
                });
            }
            ChangeScope::BaseRef if base_ref.starts_with('-') => {
                return Err(AppError::InvalidInput {
                    field: "base_ref",
                    message: format!("'{base_ref}' must not begin with '-'"),
                });
            }
            ChangeScope::BaseRef => Some(base_ref.to_string()),
            ChangeScope::Working | ChangeScope::Staged => None,
        };
        Ok(Self { scope, base_ref })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DetectChangesBudget {
    max_symbols: usize,
    batch_size: usize,
    max_depth: u32,
    deadline: Duration,
}

impl DetectChangesBudget {
    fn from_env() -> Self {
        let max_symbols = std::env::var("CIH_DETECT_CHANGES_MAX_SYMBOLS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(200);
        Self {
            max_symbols,
            batch_size: 20,
            max_depth: 4,
            deadline: blocking_timeout(),
        }
    }

    #[cfg(test)]
    fn fixed(max_symbols: usize, batch_size: usize, max_depth: u32) -> Self {
        Self {
            max_symbols,
            batch_size,
            max_depth,
            deadline: Duration::from_secs(5),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ChangeDetectionService {
    changed_files: Arc<dyn ChangedFilesSource>,
    budget: DetectChangesBudget,
}

impl ChangeDetectionService {
    pub(crate) fn new(changed_files: Arc<dyn ChangedFilesSource>) -> Self {
        Self {
            changed_files,
            budget: DetectChangesBudget::from_env(),
        }
    }

    #[cfg(test)]
    fn with_dependencies(
        changed_files: Arc<dyn ChangedFilesSource>,
        budget: DetectChangesBudget,
    ) -> Self {
        Self {
            changed_files,
            budget,
        }
    }
}

fn blocking_error(error: BlockingError) -> AppError {
    AppError::Unavailable {
        dependency: "blocking runtime",
        message: error.to_string(),
        retryable: true,
    }
}

fn graph_error(operation: &'static str, error: cih_graph_store::GraphStoreError) -> AppError {
    AppError::Unavailable {
        dependency: "graph store",
        message: format!("{operation}: {error}"),
        retryable: true,
    }
}

fn git_error(error: String) -> AppError {
    AppError::Unavailable {
        dependency: "git",
        message: error,
        retryable: false,
    }
}

/// Deterministic candidate order: sort by NodeId and drop duplicates, so the
/// budgeted prefix is stable regardless of store response order and a symbol
/// changed in several files is traversed once.
fn canonicalize_candidates(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    nodes.dedup_by(|a, b| a.id.as_str() == b.id.as_str());
    nodes
}

/// Accounting for a budgeted run: `attempted = min(total, budget)` traversals
/// ran, of which `failed` errored; the rest of `total` was omitted. Invariant:
/// `complete == (omitted + failed == 0)` — a response can never claim
/// completeness while any candidate was skipped.
fn completeness(total: usize, attempted: usize, failed: usize) -> Completeness {
    Completeness::from_work(total, attempted, failed)
}

#[derive(Debug, Serialize)]
pub(crate) struct ChangedSymbol {
    id: String,
    kind: String,
    name: String,
    file: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DetectChangesOutput {
    changed_files: Vec<String>,
    changed_symbols: Vec<ChangedSymbol>,
    affected_symbols: Vec<String>,
    affected_processes: Vec<String>,
    /// Risk from the traversals that ran — a lower bound whenever
    /// `completeness.complete` is false (never inflated; see the design record).
    risk: &'static str,
    /// Explicit alias of `risk` naming its lower-bound semantics for clients.
    risk_lower_bound: &'static str,
    /// False when any candidate was omitted (budget) or failed.
    risk_complete: bool,
    /// True whenever any changed symbol's blast radius was not computed.
    partial: bool,
    /// Changed symbols with no computed blast radius (omitted + failed).
    incomplete_symbols: usize,
    completeness: Completeness,
}

impl ChangeDetectionService {
    pub(crate) async fn execute(
        &self,
        context: &RepoContext,
        command: DetectChangesCommand,
    ) -> Result<DetectChangesOutput, AppError> {
        let store = &context.store;
        let repo_path = context.repo.canonical_path.display().to_string();
        let changed_files_source = self.changed_files.clone();
        let scope = command.scope;
        let base_ref = command.base_ref;
        let repo_for_git = repo_path.clone();
        let changed_files = run_blocking(self.budget.deadline, "git diff", move || {
            changed_files_source.changed_files(&repo_for_git, scope, base_ref.as_deref())
        })
        .await
        .map_err(blocking_error)?
        .map_err(git_error)?;

        if changed_files.is_empty() {
            return Ok(DetectChangesOutput {
                changed_files,
                changed_symbols: vec![],
                affected_symbols: vec![],
                affected_processes: vec![],
                risk: "none",
                risk_lower_bound: "none",
                risk_complete: true,
                partial: false,
                incomplete_symbols: 0,
                completeness: completeness(0, 0, 0),
            });
        }

        let changed_nodes = canonicalize_candidates(
            store
                .nodes_in_files(&changed_files)
                .await
                .map_err(|error| graph_error("resolve changed files", error))?,
        );

        // Budgeted, batched blast-radius fan-out: at most TRAVERSAL_BATCH
        // traversals in flight, candidates beyond the budget accounted as omitted.
        // Results merge into a set, so completion order is moot.
        let attempted = changed_nodes.len().min(self.budget.max_symbols);
        let mut affected_set: HashSet<String> = HashSet::new();
        let mut failed_traversals = 0usize;
        for batch in changed_nodes[..attempted].chunks(self.budget.batch_size) {
            let mut set = tokio::task::JoinSet::new();
            for node in batch {
                let store = store.clone();
                let id = node.id.clone();
                let max_depth = self.budget.max_depth;
                set.spawn(async move { store.impact(&id, Direction::Upstream, max_depth).await });
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(impact)) => {
                        for n in &impact.affected {
                            affected_set.insert(n.id.to_string());
                        }
                    }
                    _ => failed_traversals += 1,
                }
            }
        }
        for node in &changed_nodes {
            affected_set.remove(node.id.as_str());
        }
        let mut affected_symbols: Vec<String> = affected_set.into_iter().collect();
        affected_symbols.sort();

        let changed_ids: Vec<cih_core::NodeId> =
            changed_nodes.iter().map(|n| n.id.clone()).collect();
        let affected_processes = store
            .processes_for_symbols(&changed_ids)
            .await
            .map_err(|error| graph_error("resolve affected processes", error))?;

        let comp = completeness(changed_nodes.len(), attempted, failed_traversals);
        let risk = cih_graph_store::risk_from_fanout(affected_symbols.len());

        let changed_symbols: Vec<ChangedSymbol> = changed_nodes
            .iter()
            .map(|n| ChangedSymbol {
                id: n.id.to_string(),
                kind: n.kind.label().to_string(),
                name: n.name.clone(),
                file: n.file.clone(),
            })
            .collect();

        Ok(DetectChangesOutput {
            changed_files,
            changed_symbols,
            affected_symbols,
            affected_processes,
            risk,
            risk_lower_bound: risk,
            risk_complete: comp.complete,
            partial: !comp.complete,
            incomplete_symbols: comp.omitted + comp.failed,
            completeness: comp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{NodeId, NodeKind, RegistryEntry};

    struct FixedChangedFiles {
        files: Vec<String>,
    }

    impl ChangedFilesSource for FixedChangedFiles {
        fn changed_files(
            &self,
            _repo_path: &str,
            _scope: ChangeScope,
            _base_ref: Option<&str>,
        ) -> Result<Vec<String>, String> {
            Ok(self.files.clone())
        }
    }

    fn context(repo_path: &std::path::Path) -> RepoContext {
        let store = cih_store_factory::connect_store(
            "falkor",
            "redis://127.0.0.1:6380",
            "cih_change_detection_test",
            &cih_store_factory::StoreOptions::default(),
        )
        .expect("lazy graph store");
        RepoContext {
            repo: crate::domain::repository::ResolvedRepo::from_entry(RegistryEntry {
                name: "fixture".into(),
                path: repo_path.display().to_string(),
                graph_key: "cih_change_detection_test".into(),
                artifacts_dir: String::new(),
                community_artifacts_dir: None,
                indexed_at: String::new(),
                last_git_head: None,
                stats: Default::default(),
            }),
            store,
            search: Arc::new(crate::infrastructure::search_provider::SearchState::new(
                None, None,
            )),
        }
    }

    fn node(id: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Method,
            name: id.to_string(),
            qualified_name: None,
            file: String::new(),
            range: Default::default(),
            props: None,
        }
    }

    /// The S2 regression: 25 candidates under a budget of 20 must surface 5
    /// omitted symbols — the previous accounting reported `partial: false`
    /// here because it only counted failed sub-queries.
    #[test]
    fn over_budget_candidates_are_reported_omitted() {
        let c = completeness(25, 20, 0);
        assert!(!c.complete);
        assert_eq!(c.total_candidates, 25);
        assert_eq!(c.analyzed, 20);
        assert_eq!(c.omitted, 5);
        assert_eq!(c.failed, 0);
        assert_eq!(c.reasons, vec!["symbol_budget"]);
    }

    #[test]
    fn failed_traversals_are_reported() {
        let c = completeness(10, 10, 3);
        assert!(!c.complete);
        assert_eq!(c.analyzed, 7);
        assert_eq!(c.omitted, 0);
        assert_eq!(c.failed, 3);
        assert_eq!(c.reasons, vec!["traversal_failed"]);
    }

    #[test]
    fn full_analysis_is_complete() {
        let c = completeness(10, 10, 0);
        assert!(c.complete);
        assert!(c.reasons.is_empty());
        assert_eq!(c.analyzed, 10);
    }

    /// Design-record invariant: no result may claim completeness while
    /// `omitted + failed > 0`, and the three buckets always partition the
    /// candidate set.
    #[test]
    fn never_complete_when_anything_was_skipped() {
        for total in [0usize, 1, 19, 20, 21, 200, 201, 350] {
            for attempted in 0..=total.min(30) {
                for failed in 0..=attempted {
                    let c = completeness(total, attempted, failed);
                    let skipped = (total - attempted) + failed;
                    assert_eq!(
                        c.complete,
                        skipped == 0,
                        "total={total} attempted={attempted} failed={failed}"
                    );
                    assert_eq!(c.analyzed + c.omitted + c.failed, total);
                }
            }
        }
    }

    #[test]
    fn candidates_sort_and_dedup_deterministically() {
        let out =
            canonicalize_candidates(vec![node("b"), node("a"), node("b"), node("c"), node("a")]);
        let ids: Vec<&str> = out.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn command_requires_safe_base_ref() {
        let missing =
            DetectChangesCommand::try_new(ChangeScope::BaseRef, String::new()).unwrap_err();
        assert!(matches!(
            missing,
            AppError::InvalidInput {
                field: "base_ref",
                ..
            }
        ));

        let option_like =
            DetectChangesCommand::try_new(ChangeScope::BaseRef, "--output=/tmp/pwn".into())
                .unwrap_err();
        assert!(option_like.to_string().contains("must not begin with '-'"));

        let working =
            DetectChangesCommand::try_new(ChangeScope::Working, "ignored".into()).unwrap();
        assert_eq!(working.scope, ChangeScope::Working);
        assert_eq!(working.base_ref, None);
    }

    #[tokio::test]
    async fn service_returns_typed_empty_change_report_without_graph_queries() {
        let repo = tempfile::tempdir().unwrap();
        let service = ChangeDetectionService::with_dependencies(
            Arc::new(FixedChangedFiles { files: Vec::new() }),
            DetectChangesBudget::fixed(10, 2, 4),
        );
        let command = DetectChangesCommand::try_new(ChangeScope::Working, String::new()).unwrap();

        let output = service
            .execute(&context(repo.path()), command)
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_value(output).unwrap(),
            serde_json::json!({
                "changed_files": [],
                "changed_symbols": [],
                "affected_symbols": [],
                "affected_processes": [],
                "risk": "none",
                "risk_lower_bound": "none",
                "risk_complete": true,
                "partial": false,
                "incomplete_symbols": 0,
                "completeness": {
                    "complete": true,
                    "total_candidates": 0,
                    "analyzed": 0,
                    "omitted": 0,
                    "failed": 0,
                    "reasons": [],
                },
            })
        );
    }
}
