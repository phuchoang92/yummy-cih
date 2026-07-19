use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegistryStats {
    pub nodes: usize,
    pub edges: usize,
    pub files: usize,
    pub routes: usize,
    pub communities: usize,
    pub processes: usize,
    /// Index *quality*, not just size — persisted so `status` can answer "is this
    /// graph any good?". Previously computed by analyze and then dropped on the
    /// floor, which is part of why a near-zero-coverage index looked healthy.
    /// `#[serde(default)]`: entries written before these existed still load.
    #[serde(default)]
    pub resolved_edges: usize,
    #[serde(default)]
    pub unresolved_refs: u64,
    /// Emitted callable nodes ÷ callables in the AST. `None` when unmeasured (no
    /// provider in scope opts in, or the run was a cached no-op).
    #[serde(default)]
    pub callable_coverage: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub path: String,
    pub graph_key: String,
    pub artifacts_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_artifacts_dir: Option<String>,
    pub indexed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_git_head: Option<String>,
    pub stats: RegistryStats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub entries: Vec<RegistryEntry>,
}

fn registry_path() -> Option<std::path::PathBuf> {
    crate::cih_home().map(|dir| dir.join("registry.json"))
}

/// Current time as RFC-3339 UTC (no external dep required).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix_secs_to_rfc3339(secs)
}

#[doc(hidden)]
pub fn unix_secs_to_rfc3339(secs: u64) -> String {
    let tod = secs % 86400;
    let mut days = secs / 86400;
    let h = tod / 3600;
    let m = (tod / 60) % 60;
    let s = tod % 60;
    let mut y = 1970u64;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let mut mo = 1u64;
    loop {
        let dim = month_days(mo, y);
        if days < dim {
            break;
        }
        days -= dim;
        mo += 1;
    }
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z", d = days + 1)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn month_days(m: u64, y: u64) -> u64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Returns the current git HEAD SHA for the given repo path, or None.
pub fn git_head(repo_path: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Returns the list of files changed between `since_ref` and HEAD (`git diff --name-only <ref>`).
/// Returns an empty vec when git is unavailable or the ref is invalid.
pub fn git_changed_files(repo_path: &Path, since_ref: &str) -> Vec<String> {
    // Refuse refs that could be parsed as git options (e.g. `--output=…`) and
    // terminate the option list with `--` so the ref is always treated as a ref.
    if since_ref.starts_with('-') {
        return vec![];
    }
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", since_ref, "--"])
        .current_dir(repo_path)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

struct RegistryCache {
    mtime: Option<std::time::SystemTime>,
    registry: std::sync::Arc<Registry>,
}

static REGISTRY_CACHE: std::sync::OnceLock<std::sync::RwLock<Option<RegistryCache>>> =
    std::sync::OnceLock::new();

impl Registry {
    pub fn load() -> Self {
        registry_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Like [`load`](Self::load), but returns a shared snapshot cached on the
    /// registry file's mtime. The file is small yet read+parsed on every MCP tool
    /// call (via `resolve`); this skips the re-parse when it hasn't changed. Any
    /// [`save`](Self::save) bumps the mtime, so cached readers pick up writes.
    /// Use this only on read-only paths; mutating callers must use `load` + `save`.
    pub fn load_cached() -> std::sync::Arc<Registry> {
        let cache = REGISTRY_CACHE.get_or_init(|| std::sync::RwLock::new(None));
        let current_mtime = registry_path()
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
            *guard = Some(RegistryCache {
                mtime: current_mtime,
                registry: registry.clone(),
            });
        }
        registry
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path =
            registry_path().ok_or_else(|| anyhow!("cannot determine HOME for registry path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let encoded = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, encoded.as_bytes())?;
        Ok(())
    }

    /// Insert or replace an entry matched by `path`.
    pub fn upsert(&mut self, entry: RegistryEntry) {
        if let Some(pos) = self.entries.iter().position(|e| e.path == entry.path) {
            self.entries[pos] = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn find(&self, name_or_path: &str) -> Option<&RegistryEntry> {
        self.entries
            .iter()
            .find(|e| e.name == name_or_path || e.path == name_or_path)
    }

    pub fn find_mut(&mut self, name_or_path: &str) -> Option<&mut RegistryEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.name == name_or_path || e.path == name_or_path)
    }

    /// Returns true if the repo's current git HEAD differs from the indexed HEAD.
    pub fn is_stale(&self, name_or_path: &str) -> bool {
        let Some(entry) = self.find(name_or_path) else {
            return true;
        };
        let current = git_head(Path::new(&entry.path));
        match (&entry.last_git_head, current) {
            (Some(saved), Some(cur)) => saved != &cur,
            _ => false,
        }
    }
}

#[cfg(test)]
mod git_arg_tests {
    use super::git_changed_files;
    use std::path::Path;

    #[test]
    fn git_changed_files_refuses_option_like_ref() {
        // Leading-dash refs (e.g. `--output=…`) are refused so git can't be driven
        // into writing a file; returns empty rather than shelling out.
        assert!(git_changed_files(Path::new("."), "--output=/tmp/pwn").is_empty());
    }

    #[test]
    fn registry_path_composes_from_cih_home() {
        // registry.json lives under ~/.cih; verify the composition without
        // depending on HOME being set (skips when it isn't).
        if let Some(home) = crate::cih_home() {
            assert_eq!(super::registry_path(), Some(home.join("registry.json")));
        }
    }
}
