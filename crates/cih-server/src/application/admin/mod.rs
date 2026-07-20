//! Repository catalog and administrative query use cases.

pub(crate) mod resolve_patterns;

use serde::Serialize;

use crate::application::app_services::RepoContextService;
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;

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

    pub(crate) fn list_repos(&self) -> ListReposOutput {
        let catalog = self.repos.catalog_snapshot();
        let registry = catalog.registry();
        if let Some(group_name) = &self.group {
            if let Some(group) = catalog.groups().find(group_name) {
                let repos = registry
                    .entries
                    .iter()
                    .filter(|entry| group.repos.iter().any(|name| name == &entry.name))
                    .cloned()
                    .collect();
                return ListReposOutput::Group(GroupRepoList {
                    group: group_name.clone(),
                    primary_graph_key: self.graph_key.clone(),
                    repos,
                });
            }
        }
        ListReposOutput::Entries(registry.entries.clone())
    }

    pub(crate) fn status(&self, command: RepoStatusCommand) -> Result<RepoStatusOutput, AppError> {
        let catalog = self.repos.catalog_snapshot();
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
                    contracts_synced_at: state.as_ref().map(|value| value.synced_at.clone()),
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
    }
}

pub(crate) struct RepoStatusCommand {
    pub(crate) name: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ListReposOutput {
    Entries(Vec<cih_core::RegistryEntry>),
    Group(GroupRepoList),
}

#[derive(Debug, Serialize)]
pub(crate) struct GroupRepoList {
    pub(crate) group: String,
    pub(crate) primary_graph_key: String,
    pub(crate) repos: Vec<cih_core::RegistryEntry>,
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
