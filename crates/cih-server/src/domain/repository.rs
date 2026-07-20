//! Repository identity and immutable catalog snapshots.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::error::AppError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepoSelector {
    Default,
    NameOrPath(String),
}

impl RepoSelector {
    pub(crate) fn from_wire(value: &str) -> Self {
        if value.trim().is_empty() {
            Self::Default
        } else {
            Self::NameOrPath(value.trim().to_string())
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedRepo {
    pub(crate) registry_entry: cih_core::RegistryEntry,
    pub(crate) canonical_path: PathBuf,
    pub(crate) versioned_artifacts_dir: Option<PathBuf>,
    pub(crate) community_artifacts_dir: Option<PathBuf>,
}

impl ResolvedRepo {
    pub(crate) fn from_entry(entry: cih_core::RegistryEntry) -> Self {
        let canonical_path = normalize_path(Path::new(&entry.path));
        let versioned_artifacts_dir = nonempty_path(&entry.artifacts_dir).map(normalize_path);
        let community_artifacts_dir = entry
            .community_artifacts_dir
            .as_deref()
            .and_then(nonempty_path)
            .map(normalize_path);
        Self {
            registry_entry: entry,
            canonical_path,
            versioned_artifacts_dir,
            community_artifacts_dir,
        }
    }

    pub(crate) fn graph_key(&self) -> &str {
        &self.registry_entry.graph_key
    }

    pub(crate) fn artifacts_root(&self) -> Option<PathBuf> {
        self.versioned_artifacts_dir
            .as_deref()
            .map(unversioned_artifacts_dir)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RepoCatalogSnapshot {
    primary_graph_key: String,
    registry: Arc<cih_core::Registry>,
    groups: Arc<cih_core::GroupRegistry>,
}

impl RepoCatalogSnapshot {
    pub(crate) fn new(
        primary_graph_key: String,
        registry: Arc<cih_core::Registry>,
        groups: Arc<cih_core::GroupRegistry>,
    ) -> Self {
        Self {
            primary_graph_key,
            registry,
            groups,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        primary_graph_key: String,
        registry: cih_core::Registry,
        groups: cih_core::GroupRegistry,
    ) -> Self {
        Self::new(primary_graph_key, Arc::new(registry), Arc::new(groups))
    }

    pub(crate) fn resolve(&self, selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
        resolve_entry(&self.registry, &selector, &self.primary_graph_key)
            .cloned()
            .map(ResolvedRepo::from_entry)
    }

    pub(crate) fn registry(&self) -> &cih_core::Registry {
        &self.registry
    }

    pub(crate) fn groups(&self) -> &cih_core::GroupRegistry {
        &self.groups
    }
}

pub(crate) fn resolve_entry<'a>(
    registry: &'a cih_core::Registry,
    selector: &RepoSelector,
    primary_graph_key: &str,
) -> Result<&'a cih_core::RegistryEntry, AppError> {
    if registry.entries.is_empty() {
        return Err(AppError::InvalidInput {
            field: "repo",
            message: "no repos in registry; run `cih-engine analyze <repo>` first".into(),
        });
    }
    match selector {
        RepoSelector::Default => registry
            .entries
            .iter()
            .find(|entry| entry.graph_key == primary_graph_key)
            .ok_or_else(|| AppError::InvalidInput {
                field: "repo",
                message: format!(
                    "no repo registered for graph_key '{primary_graph_key}'; pass `repo` explicitly"
                ),
            }),
        RepoSelector::NameOrPath(value) => registry.find(value).ok_or_else(|| AppError::NotFound {
            entity: "repo",
            key: value.clone(),
        }),
    }
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

pub(crate) fn unversioned_artifacts_dir(versioned: &Path) -> PathBuf {
    versioned
        .parent()
        .map(normalize_path)
        .unwrap_or_else(|| normalize_path(versioned))
}

fn nonempty_path(value: &str) -> Option<&Path> {
    (!value.trim().is_empty()).then(|| Path::new(value))
}
