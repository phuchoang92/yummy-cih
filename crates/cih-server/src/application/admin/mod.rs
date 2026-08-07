//! Repository catalog and administrative query use cases.

pub(crate) mod resolve_patterns;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::application::app_services::RepoContextService;
use crate::application::cursor::{
    canonical_filter_hash, repository_cursor_identity, unix_now, CursorCodec,
    DEFAULT_CURSOR_TTL_SECS,
};
use crate::domain::error::AppError;
use crate::domain::indexing::IndexQueueMetrics;
use crate::domain::repository::RepoSelector;
use crate::ports::blocking_runtime::{blocking_metrics, BlockingMetricsSnapshot};
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking};
use crate::ports::job_scheduler::IndexJobScheduler;
use crate::ports::retrieval_metrics::{RetrievalMetricsProvider, RetrievalMetricsSnapshot};

pub(crate) const LIST_REPOS_DEFAULT_LIMIT: usize = 50;
pub(crate) const LIST_REPOS_MAX_LIMIT: usize = 200;
/// Compatibility ceiling for the unpaged `list_repos` tool. The tool also
/// enforces [`LEGACY_LIST_REPOS_WIRE_BYTES`] after MCP encoding.
pub(crate) const LEGACY_LIST_REPOS_COUNT_CAP: usize = 200;
/// Maximum uncompressed serialized MCP `CallToolResult` accepted from the
/// legacy tool. Clients that exceed either legacy ceiling must page through
/// `list_repos_page`.
pub(crate) const LEGACY_LIST_REPOS_WIRE_BYTES: usize = 256 * 1024;

const LIST_REPOS_CURSOR_SCHEMA: u8 = 2;
const LIST_REPOS_CURSOR_OPERATION: &str = "list_repos_page";

#[derive(Clone)]
pub(crate) struct OperationalMetricsService {
    scheduler: Arc<dyn IndexJobScheduler>,
    retrieval: Arc<dyn RetrievalMetricsProvider>,
}

impl OperationalMetricsService {
    pub(crate) fn new(
        scheduler: Arc<dyn IndexJobScheduler>,
        retrieval: Arc<dyn RetrievalMetricsProvider>,
    ) -> Self {
        Self {
            scheduler,
            retrieval,
        }
    }

    pub(crate) async fn snapshot(&self) -> OperationalMetricsOutput {
        let (index_queue, retrieval) =
            tokio::join!(self.scheduler.metrics(), self.retrieval.snapshot());
        OperationalMetricsOutput {
            blocking: blocking_metrics(),
            index_queue,
            retrieval,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OperationalMetricsOutput {
    pub(crate) blocking: BlockingMetricsSnapshot,
    pub(crate) index_queue: IndexQueueMetrics,
    pub(crate) retrieval: RetrievalMetricsSnapshot,
}

#[derive(Clone)]
pub(crate) struct RepositoryAdminService {
    repos: RepoContextService,
    graph_key: String,
    group: Option<String>,
}

impl RepositoryAdminService {
    pub(crate) fn new(repos: RepoContextService, graph_key: String, group: Option<String>) -> Self {
        Self {
            repos,
            graph_key,
            group,
        }
    }

    pub(crate) fn list_repos(&self) -> Result<ListReposOutput, LegacyListReposError> {
        let catalog = self.repos.catalog_snapshot();
        let registry = catalog.registry();
        if let Some(group_name) = &self.group {
            if let Some(group) = catalog.groups().find(group_name) {
                let repo_count = registry
                    .entries
                    .iter()
                    .filter(|entry| group.repos.iter().any(|name| name == &entry.name))
                    .count();
                enforce_legacy_count(repo_count)?;
                let repos = registry
                    .entries
                    .iter()
                    .filter(|entry| group.repos.iter().any(|name| name == &entry.name))
                    .cloned()
                    .collect();
                return Ok(ListReposOutput::Group(GroupRepoList {
                    group: group_name.clone(),
                    primary_graph_key: self.graph_key.clone(),
                    repos,
                }));
            }
        }
        enforce_legacy_count(registry.entries.len())?;
        Ok(ListReposOutput::Entries(registry.entries.clone()))
    }

    pub(crate) async fn list_repos_page(
        &self,
        command: ListReposPageCommand,
    ) -> Result<ListReposPageOutput, AppError> {
        let catalog = self.repos.catalog_snapshot();
        let group_scope = self.group.as_ref().and_then(|group_name| {
            catalog.groups().find(group_name).map(|group| GroupScope {
                name: group_name.clone(),
                repos: group.repos.iter().cloned().collect(),
            })
        });
        let primary_graph_key = self.graph_key.clone();
        let cursor_codec = CursorCodec::global()?.clone();
        run_blocking(blocking_timeout(), "repository list page", move || {
            let snapshot =
                cih_core::Registry::load_snapshot().map_err(|error| AppError::Unavailable {
                    dependency: "registry",
                    message: error.to_string(),
                    retryable: true,
                })?;
            build_list_repos_page(
                snapshot,
                group_scope.as_ref(),
                &primary_graph_key,
                command,
                &cursor_codec,
                unix_now(),
            )
        })
        .await
        .map_err(|error| AppError::Unavailable {
            dependency: "repository list",
            message: error.to_string(),
            retryable: true,
        })?
    }

    pub(crate) async fn status(
        &self,
        command: RepoStatusCommand,
    ) -> Result<RepoStatusOutput, AppError> {
        let catalog = self.repos.catalog_snapshot();
        run_blocking(
            blocking_timeout(),
            "repository status sidecars",
            move || {
                let repo = catalog.resolve(RepoSelector::NameOrPath(command.name))?;
                let registry = catalog.registry();
                let entry = repo.registry_entry;
                let stale = registry.is_stale(&entry.name);
                let groups = catalog
                    .groups()
                    .groups_containing(&entry.name)
                    .map(|group| {
                        let state = cih_core::group_dir(&group.name)
                            .and_then(|directory| cih_core::SyncState::load(&directory));
                        let contracts_exist =
                            cih_core::contracts_path(&group.name).is_some_and(|path| path.exists());
                        GroupSyncStatus {
                            name: group.name.clone(),
                            contracts_synced_at: state
                                .as_ref()
                                .map(|value| value.synced_at.clone()),
                            stale: cih_core::group_contracts_stale(
                                group,
                                registry,
                                state.as_ref(),
                                contracts_exist,
                            ),
                        }
                    })
                    .collect();
                Ok(RepoStatusOutput {
                    entry,
                    stale,
                    groups,
                })
            },
        )
        .await
        .map_err(|error| AppError::Unavailable {
            dependency: "repository status",
            message: error.to_string(),
            retryable: true,
        })?
    }
}

pub(crate) struct RepoStatusCommand {
    pub(crate) name: String,
}

pub(crate) struct ListReposPageCommand {
    pub(crate) filter: String,
    pub(crate) limit: usize,
    pub(crate) cursor: Option<String>,
}

#[derive(Clone)]
struct GroupScope {
    name: String,
    repos: HashSet<String>,
}

#[derive(Debug)]
pub(crate) struct LegacyListReposError {
    pub(crate) actual_count: usize,
    pub(crate) count_cap: usize,
}

impl std::fmt::Display for LegacyListReposError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "legacy list_repos has {} entries, exceeding its exact-result cap of {}; use list_repos_page",
            self.actual_count, self.count_cap
        )
    }
}

fn enforce_legacy_count(actual_count: usize) -> Result<(), LegacyListReposError> {
    if actual_count > LEGACY_LIST_REPOS_COUNT_CAP {
        return Err(LegacyListReposError {
            actual_count,
            count_cap: LEGACY_LIST_REPOS_COUNT_CAP,
        });
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ListReposOutput {
    Entries(Vec<cih_core::RegistryEntry>),
    Group(GroupRepoList),
}

impl ListReposOutput {
    pub(crate) fn repo_count(&self) -> usize {
        match self {
            Self::Entries(entries) => entries.len(),
            Self::Group(group) => group.repos.len(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct GroupRepoList {
    pub(crate) group: String,
    pub(crate) primary_graph_key: String,
    pub(crate) repos: Vec<cih_core::RegistryEntry>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepoListStatus {
    Current,
    Stale,
    Missing,
    Unknown,
}

#[derive(Debug, Serialize)]
pub(crate) struct RepoListItem {
    #[serde(flatten)]
    pub(crate) entry: cih_core::RegistryEntry,
    pub(crate) status: RepoListStatus,
    pub(crate) stale: bool,
    pub(crate) stale_known: bool,
    pub(crate) missing: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListReposPageOutput {
    pub(crate) version: u8,
    pub(crate) repos: Vec<RepoListItem>,
    pub(crate) returned: usize,
    pub(crate) total_matching: usize,
    pub(crate) total_exact: bool,
    pub(crate) has_more: bool,
    pub(crate) next_cursor: Option<String>,
    pub(crate) limit: usize,
    pub(crate) filter: String,
    pub(crate) registry_revision: cih_core::RegistryRevision,
    pub(crate) registry_recovered_from_backup: bool,
    pub(crate) group: Option<String>,
    pub(crate) primary_graph_key: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ListReposCursor {
    filter_hash: String,
    scope: String,
    limit: usize,
    registry_sequence: u64,
    registry_digest: String,
    last_name: String,
    last_repository_identity: String,
}

fn build_list_repos_page(
    snapshot: cih_core::RegistrySnapshot,
    group_scope: Option<&GroupScope>,
    primary_graph_key: &str,
    command: ListReposPageCommand,
    cursor_codec: &CursorCodec,
    now: u64,
) -> Result<ListReposPageOutput, AppError> {
    let limit = match command.limit {
        0 => LIST_REPOS_DEFAULT_LIMIT,
        value if value <= LIST_REPOS_MAX_LIMIT => value,
        value => {
            return Err(AppError::InvalidInput {
                field: "limit",
                message: format!(
                    "{value} exceeds the list_repos_page maximum of {LIST_REPOS_MAX_LIMIT}"
                ),
            });
        }
    };
    let filter = command.filter.trim().to_lowercase();
    if filter.len() > 512 {
        return Err(AppError::InvalidInput {
            field: "filter",
            message: "must be at most 512 UTF-8 bytes".to_string(),
        });
    }
    let scope = group_scope
        .map(|group| format!("group:{}", group.name))
        .unwrap_or_else(|| "all".to_string());
    let filter_hash = canonical_filter_hash(filter.as_bytes());
    let cursor = command
        .cursor
        .as_deref()
        .map(|raw| {
            cursor_codec
                .decode_at(raw, LIST_REPOS_CURSOR_OPERATION, now)
                .map_err(|error| error.into_app_error("cursor"))
        })
        .transpose()?;
    if let Some(cursor) = cursor.as_ref() {
        validate_list_repos_cursor(cursor, &filter_hash, &scope, limit, &snapshot.revision)?;
    }

    let mut entries: Vec<_> = snapshot
        .registry
        .entries
        .iter()
        .filter(|entry| {
            group_scope.is_none_or(|group| group.repos.contains(&entry.name))
                && (filter.is_empty()
                    || entry.name.to_lowercase().contains(&filter)
                    || entry.path.to_lowercase().contains(&filter))
        })
        .cloned()
        .map(|entry| (repository_cursor_identity(&entry), entry))
        .collect();
    entries.sort_by(|left, right| {
        left.1
            .name
            .cmp(&right.1.name)
            .then_with(|| left.0.cmp(&right.0))
    });
    let total_matching = entries.len();
    let start = cursor
        .as_ref()
        .map(|cursor| {
            entries.partition_point(|(identity, entry)| {
                (&entry.name, identity) <= (&cursor.last_name, &cursor.last_repository_identity)
            })
        })
        .unwrap_or(0);
    let end = start.saturating_add(limit).min(entries.len());
    let has_more = end < entries.len();
    let page_entries = &entries[start..end];
    let next_cursor = if has_more {
        page_entries.last().map(|(identity, last)| {
            cursor_codec
                .encode_at(
                    LIST_REPOS_CURSOR_OPERATION,
                    DEFAULT_CURSOR_TTL_SECS,
                    &ListReposCursor {
                        filter_hash: filter_hash.clone(),
                        scope: scope.clone(),
                        limit,
                        registry_sequence: snapshot.revision.sequence,
                        registry_digest: snapshot.revision.content_digest.clone(),
                        last_name: last.name.clone(),
                        last_repository_identity: identity.clone(),
                    },
                    now,
                )
                .map_err(|error| error.into_app_error("cursor"))
        })
    } else {
        None
    }
    .transpose()?;
    let repos = page_entries
        .iter()
        .map(|(_, entry)| repo_list_item(entry.clone()))
        .collect();
    let returned = page_entries.len();

    Ok(ListReposPageOutput {
        version: LIST_REPOS_CURSOR_SCHEMA,
        repos,
        returned,
        total_matching,
        total_exact: true,
        has_more,
        next_cursor,
        limit,
        filter,
        registry_revision: snapshot.revision,
        registry_recovered_from_backup: snapshot.recovered_from_backup,
        group: group_scope.map(|group| group.name.clone()),
        primary_graph_key: primary_graph_key.to_string(),
    })
}

fn repo_list_item(entry: cih_core::RegistryEntry) -> RepoListItem {
    let missing = !Path::new(&entry.path).is_dir();
    let current_head = (!missing)
        .then(|| cih_core::git_head(Path::new(&entry.path)))
        .flatten();
    let stale = match (entry.last_git_head.as_ref(), current_head.as_ref()) {
        (Some(indexed), Some(current)) => Some(indexed != current),
        _ => None,
    };
    let status = if missing {
        RepoListStatus::Missing
    } else {
        match stale {
            Some(true) => RepoListStatus::Stale,
            Some(false) => RepoListStatus::Current,
            None => RepoListStatus::Unknown,
        }
    };
    RepoListItem {
        entry,
        status,
        stale: stale.unwrap_or(false),
        stale_known: stale.is_some(),
        missing,
    }
}

fn validate_list_repos_cursor(
    cursor: &ListReposCursor,
    filter_hash: &str,
    scope: &str,
    limit: usize,
    revision: &cih_core::RegistryRevision,
) -> Result<(), AppError> {
    if cursor.filter_hash != filter_hash {
        return Err(cursor_error(
            "wrong_filter",
            "cursor filter differs from this request; restart from the first page",
        ));
    }
    if cursor.scope != scope {
        return Err(cursor_error(
            "wrong_scope",
            "cursor repository scope differs from this server; restart from the first page",
        ));
    }
    if cursor.limit != limit {
        return Err(cursor_error(
            "wrong_page_bounds",
            "cursor limit differs from this request; reuse the original limit",
        ));
    }
    if cursor.registry_sequence != revision.sequence
        || cursor.registry_digest != revision.content_digest
    {
        return Err(cursor_error(
            "registry_changed",
            "registry_changed: the registry changed between pages; restart from the first page",
        ));
    }
    Ok(())
}

fn cursor_error(code: &'static str, message: impl Into<String>) -> AppError {
    AppError::InvalidInput {
        field: "cursor",
        message: format!("{code}: {}", message.into()),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RepoStatusOutput {
    pub(crate) entry: cih_core::RegistryEntry,
    pub(crate) stale: bool,
    pub(crate) groups: Vec<GroupSyncStatus>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GroupSyncStatus {
    pub(crate) name: String,
    pub(crate) contracts_synced_at: Option<String>,
    pub(crate) stale: bool,
}

#[cfg(test)]
mod list_repos_tests {
    use super::*;

    const TEST_KEY: [u8; 32] = [0x5a; 32];
    const TEST_NOW: u64 = 1_000_000;

    fn test_codec() -> CursorCodec {
        CursorCodec::for_test(TEST_KEY, "test-v1")
    }

    fn entry(name: &str, path: &str) -> cih_core::RegistryEntry {
        cih_core::RegistryEntry {
            repository_id: None,
            name: name.to_string(),
            path: path.to_string(),
            graph_key: format!("graph-{name}"),
            artifacts_dir: format!("/artifacts/{name}"),
            latest_artifact_version: None,
            published_artifact_version: None,
            published_graph_content_version: None,
            published_epoch: None,
            community_artifacts_dir: None,
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
            last_git_head: None,
            stats: cih_core::RegistryStats::default(),
        }
    }

    fn entry_with_id(name: &str, path: &str, digit: char) -> cih_core::RegistryEntry {
        let mut entry = entry(name, path);
        entry.repository_id = Some(
            cih_core::RepositoryId::parse(digit.to_string().repeat(64))
                .expect("test repository ID should be valid"),
        );
        entry
    }

    fn snapshot(
        entries: Vec<cih_core::RegistryEntry>,
        sequence: u64,
        digest: &str,
    ) -> cih_core::RegistrySnapshot {
        cih_core::RegistrySnapshot {
            registry: cih_core::Registry { entries },
            revision: cih_core::RegistryRevision {
                sequence,
                content_digest: digest.to_string(),
            },
            recovered_from_backup: false,
        }
    }

    fn page(
        snapshot: cih_core::RegistrySnapshot,
        filter: &str,
        limit: usize,
        cursor: Option<String>,
        now: u64,
    ) -> Result<ListReposPageOutput, AppError> {
        let codec = test_codec();
        build_list_repos_page(
            snapshot,
            None,
            "primary",
            ListReposPageCommand {
                filter: filter.to_string(),
                limit,
                cursor,
            },
            &codec,
            now,
        )
    }

    #[test]
    fn keyset_pages_are_stable_and_do_not_duplicate_entries() {
        let registry = snapshot(
            vec![
                entry("zeta", "/missing/zeta"),
                entry("alpha", "/missing/alpha"),
                entry("beta", "/missing/beta"),
            ],
            7,
            "digest-a",
        );
        let first = page(registry.clone(), "", 2, None, TEST_NOW).unwrap();
        assert_eq!(
            first
                .repos
                .iter()
                .map(|repo| repo.entry.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert!(first.has_more);
        assert_eq!(first.total_matching, 3);
        assert!(first.total_exact);

        let second = page(
            registry,
            "",
            2,
            first.next_cursor,
            TEST_NOW.saturating_add(1),
        )
        .unwrap();
        assert_eq!(second.repos.len(), 1);
        assert_eq!(second.repos[0].entry.name, "zeta");
        assert!(!second.has_more);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn same_name_uses_path_as_the_stable_tie_breaker() {
        let registry = snapshot(
            vec![
                entry_with_id("same", "/missing/b", 'b'),
                entry_with_id("same", "/missing/a", 'a'),
            ],
            1,
            "digest",
        );
        let first = page(registry.clone(), "", 1, None, TEST_NOW).unwrap();
        assert_eq!(first.repos[0].entry.path, "/missing/a");
        let second = page(registry, "", 1, first.next_cursor, TEST_NOW).unwrap();
        assert_eq!(second.repos[0].entry.path, "/missing/b");
    }

    #[test]
    fn duplicate_name_order_uses_repository_id_not_path() {
        let registry = snapshot(
            vec![
                entry_with_id("same", "/missing/a", 'b'),
                entry_with_id("same", "/missing/b", 'a'),
            ],
            1,
            "digest",
        );

        let first = page(registry.clone(), "", 1, None, TEST_NOW).unwrap();
        assert_eq!(first.repos[0].entry.path, "/missing/b");
        let second = page(registry, "", 1, first.next_cursor, TEST_NOW).unwrap();
        assert_eq!(second.repos[0].entry.path, "/missing/a");
    }

    #[test]
    fn legacy_duplicate_name_order_uses_explicit_graph_key_fallback() {
        let mut graph_z = entry("same", "/missing/a");
        graph_z.graph_key = "z-graph".to_string();
        let mut graph_a = entry("same", "/missing/b");
        graph_a.graph_key = "a-graph".to_string();
        let registry = snapshot(vec![graph_z, graph_a], 1, "digest");

        let first = page(registry.clone(), "", 1, None, TEST_NOW).unwrap();
        assert_eq!(first.repos[0].entry.path, "/missing/b");
        let second = page(registry, "", 1, first.next_cursor, TEST_NOW).unwrap();
        assert_eq!(second.repos[0].entry.path, "/missing/a");
    }

    #[test]
    fn filter_is_normalized_and_status_reports_missing_without_deleting() {
        let registry = snapshot(
            vec![
                entry("AlphaService", "/definitely/missing/alpha"),
                entry("beta", "/workspace/OTHER-service"),
            ],
            1,
            "digest",
        );
        let output = page(registry, "  ALPHA  ", 0, None, TEST_NOW).unwrap();
        assert_eq!(output.limit, LIST_REPOS_DEFAULT_LIMIT);
        assert_eq!(output.filter, "alpha");
        assert_eq!(output.repos.len(), 1);
        assert_eq!(output.repos[0].status, RepoListStatus::Missing);
        assert!(output.repos[0].missing);
        assert!(!output.repos[0].stale_known);
    }

    #[test]
    fn continuation_rejects_registry_mutation() {
        let entries = vec![entry("alpha", "/missing/a"), entry("beta", "/missing/b")];
        let first = page(
            snapshot(entries.clone(), 10, "before"),
            "",
            1,
            None,
            TEST_NOW,
        )
        .unwrap();
        let error = page(
            snapshot(entries, 11, "after"),
            "",
            1,
            first.next_cursor,
            TEST_NOW,
        )
        .unwrap_err();
        assert!(error.to_string().contains("registry_changed"));
    }

    #[test]
    fn continuation_rejects_filter_limit_tampering_and_expiry() {
        let registry = snapshot(
            vec![entry("alpha", "/missing/a"), entry("beta", "/missing/b")],
            10,
            "digest",
        );
        let first = page(registry.clone(), "", 1, None, TEST_NOW).unwrap();
        let cursor = first.next_cursor.unwrap();

        let wrong_filter =
            page(registry.clone(), "alpha", 1, Some(cursor.clone()), TEST_NOW).unwrap_err();
        assert!(wrong_filter.to_string().contains("wrong_filter"));

        let wrong_limit =
            page(registry.clone(), "", 2, Some(cursor.clone()), TEST_NOW).unwrap_err();
        assert!(wrong_limit.to_string().contains("wrong_page_bounds"));

        let mut tampered = cursor.clone().into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).unwrap();
        let tamper_error = page(registry.clone(), "", 1, Some(tampered), TEST_NOW).unwrap_err();
        assert!(tamper_error.to_string().contains("cursor_tampered"));

        let expired = page(
            registry,
            "",
            1,
            Some(cursor),
            TEST_NOW + DEFAULT_CURSOR_TTL_SECS + 1,
        )
        .unwrap_err();
        assert!(expired.to_string().contains("cursor_expired"));
    }

    #[test]
    fn page_limit_and_legacy_count_caps_fail_loudly() {
        let too_large = page(
            snapshot(Vec::new(), 1, "digest"),
            "",
            LIST_REPOS_MAX_LIMIT + 1,
            None,
            TEST_NOW,
        )
        .unwrap_err();
        assert!(too_large.to_string().contains("maximum"));

        assert!(enforce_legacy_count(LEGACY_LIST_REPOS_COUNT_CAP).is_ok());
        let legacy = enforce_legacy_count(LEGACY_LIST_REPOS_COUNT_CAP + 1).unwrap_err();
        assert_eq!(legacy.actual_count, LEGACY_LIST_REPOS_COUNT_CAP + 1);
    }
}
