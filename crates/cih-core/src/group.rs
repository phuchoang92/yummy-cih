use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::registry::Registry;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupEntry {
    pub name: String,
    /// Registry names of member repos.
    pub repos: Vec<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroupRegistry {
    pub groups: Vec<GroupEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractMatchKind {
    HttpRoute,
    KafkaTopic,
    SpringEvent,
}

impl From<crate::MessagingFramework> for ContractMatchKind {
    fn from(fw: crate::MessagingFramework) -> Self {
        match fw {
            crate::MessagingFramework::Spring => ContractMatchKind::SpringEvent,
            // Kafka + the JS topic/queue/event frameworks all match by topic name.
            crate::MessagingFramework::Kafka
            | crate::MessagingFramework::SocketIo
            | crate::MessagingFramework::Bull
            | crate::MessagingFramework::Rabbitmq
            | crate::MessagingFramework::NestMicroservice => ContractMatchKind::KafkaTopic,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContractMatch {
    pub kind: ContractMatchKind,
    pub provider_repo: String,
    pub provider_id: String,
    pub consumer_repo: String,
    pub consumer_id: String,
    pub match_key: String,
}

fn cih_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cih"))
}

fn groups_path() -> Option<PathBuf> {
    cih_home().map(|dir| dir.join("groups.json"))
}

pub fn group_dir(name: &str) -> Option<PathBuf> {
    cih_home().map(|dir| dir.join("groups").join(name))
}

pub fn contracts_path(name: &str) -> Option<PathBuf> {
    group_dir(name).map(|dir| dir.join("contracts.jsonl"))
}

const SYNC_STATE_FILE: &str = "sync-state.json";

pub fn sync_state_path(name: &str) -> Option<PathBuf> {
    group_dir(name).map(|dir| dir.join(SYNC_STATE_FILE))
}

/// Registry snapshot of one member repo taken when the group was last synced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRepoSnapshot {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub indexed_at: String,
    #[serde(default)]
    pub last_git_head: Option<String>,
}

/// Freshness stamp written next to `contracts.jsonl` on every group sync.
/// A separate file (not a header line in the jsonl) so old strict-parsing
/// readers of `contracts.jsonl` keep working.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub synced_at: String,
    /// Monotonic sync counter for this group (starts at 1).
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub repos: Vec<SyncRepoSnapshot>,
}

impl SyncState {
    /// Returns `None` when the stamp is missing or unreadable (pre-stamp syncs).
    pub fn load(group_dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(group_dir.join(SYNC_STATE_FILE)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, group_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(group_dir)?;
        let tmp = group_dir.join(format!("{SYNC_STATE_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, group_dir.join(SYNC_STATE_FILE))?;
        Ok(())
    }

    pub fn snapshot_of(entry: &crate::registry::RegistryEntry) -> SyncRepoSnapshot {
        SyncRepoSnapshot {
            name: entry.name.clone(),
            indexed_at: entry.indexed_at.clone(),
            last_git_head: entry.last_git_head.clone(),
        }
    }
}

/// Whether a group's synced contracts are stale relative to the repo registry.
///
/// Stale iff any member repo is missing from the registry, or a member's
/// `indexed_at`/`last_git_head` differs from the sync-time snapshot (including
/// members added to the group after the sync), or contracts exist without a
/// stamp (pre-stamp sync). A group that was never synced (no contracts, no
/// stamp) is not stale — there is nothing to be stale.
pub fn group_contracts_stale(
    group: &GroupEntry,
    registry: &Registry,
    state: Option<&SyncState>,
    contracts_exist: bool,
) -> bool {
    if group.repos.iter().any(|repo| registry.find(repo).is_none()) {
        return true;
    }
    let Some(state) = state else {
        return contracts_exist;
    };
    group.repos.iter().any(|repo| {
        let entry = registry
            .find(repo)
            .expect("checked above: every member resolves");
        state
            .repos
            .iter()
            .find(|snap| snap.name == entry.name)
            .is_none_or(|snap| {
                snap.indexed_at != entry.indexed_at || snap.last_git_head != entry.last_git_head
            })
    })
}

/// Normalize a URL path for cross-repo contract matching.
/// Strips query params, strips scheme + host, lowercases literal segments,
/// and replaces `{var}` / `:var` path variables with `{*}`.
pub fn normalize_contract_path(path: &str) -> String {
    let mut base = path.split('?').next().unwrap_or(path).trim().to_string();
    if let Some(idx) = base.find("://") {
        let after_scheme = &base[idx + 3..];
        base = after_scheme
            .find('/')
            .map(|slash| after_scheme[slash..].to_string())
            .unwrap_or_else(|| "/".to_string());
    }
    if base.is_empty() {
        base = "/".to_string();
    }
    if !base.starts_with('/') {
        base = format!("/{base}");
    }
    let segments: Vec<String> = base
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if (segment.starts_with('{') && segment.ends_with('}')) || segment.starts_with(':') {
                "{*}".to_string()
            } else {
                segment.to_ascii_lowercase()
            }
        })
        .collect();
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

struct GroupRegistryCache {
    mtime: Option<std::time::SystemTime>,
    registry: std::sync::Arc<GroupRegistry>,
}

static GROUP_REGISTRY_CACHE: std::sync::OnceLock<std::sync::RwLock<Option<GroupRegistryCache>>> =
    std::sync::OnceLock::new();

impl GroupRegistry {
    pub fn load() -> Self {
        groups_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Like [`load`](Self::load), but returns a shared snapshot cached on the
    /// groups.json mtime — the read-only twin of [`Registry::load_cached`]. Any
    /// [`save`](Self::save) bumps the mtime, so cached readers pick up writes.
    /// Use this only on read-only paths; mutating callers must use `load` + `save`.
    pub fn load_cached() -> std::sync::Arc<GroupRegistry> {
        let cache = GROUP_REGISTRY_CACHE.get_or_init(|| std::sync::RwLock::new(None));
        let current_mtime = groups_path()
            .and_then(|p| std::fs::metadata(&p).ok())
            .and_then(|m| m.modified().ok());
        if let Ok(guard) = cache.read() {
            if let Some(cached) = guard.as_ref() {
                if cached.mtime == current_mtime {
                    return cached.registry.clone();
                }
            }
        }
        let registry = std::sync::Arc::new(Self::load());
        if let Ok(mut guard) = cache.write() {
            *guard = Some(GroupRegistryCache {
                mtime: current_mtime,
                registry: registry.clone(),
            });
        }
        registry
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = groups_path().ok_or_else(|| anyhow!("cannot determine HOME for group path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn find(&self, name: &str) -> Option<&GroupEntry> {
        self.groups.iter().find(|group| group.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut GroupEntry> {
        self.groups.iter_mut().find(|group| group.name == name)
    }

    pub fn upsert(&mut self, entry: GroupEntry) {
        if let Some(pos) = self
            .groups
            .iter()
            .position(|group| group.name == entry.name)
        {
            self.groups[pos] = entry;
        } else {
            self.groups.push(entry);
            self.groups.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.groups.len();
        self.groups.retain(|group| group.name != name);
        self.groups.len() != before
    }

    /// Groups that list `repo_name` as a member.
    pub fn groups_containing<'a>(
        &'a self,
        repo_name: &'a str,
    ) -> impl Iterator<Item = &'a GroupEntry> {
        self.groups
            .iter()
            .filter(move |group| group.repos.iter().any(|repo| repo == repo_name))
    }
}
