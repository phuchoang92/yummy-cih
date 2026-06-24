use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractMatchKind {
    HttpRoute,
    KafkaTopic,
    SpringEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl GroupRegistry {
    pub fn load() -> Self {
        groups_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
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
}

