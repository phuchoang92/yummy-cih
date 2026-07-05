use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::entry::FeatureGroupEntry;

/// `.cih/feature-overrides.json` — human-edited sidecar for locking node assignments.
/// Never written by automated runs; only merged into `groups.jsonl` during discover.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeatureOverrides {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<FeatureOverrideEntry>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureOverrideEntry {
    pub node_id: String,
    pub feature: String,
    #[serde(default)]
    pub reason: String,
}

impl FeatureOverrides {
    /// Load from `<repo>/.cih/feature-overrides.json`. Returns `None` when absent or malformed.
    pub fn load(repo: &Path) -> Option<Self> {
        let path = repo.join(".cih").join("feature-overrides.json");
        let text = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Path to the overrides sidecar for `repo`.
    pub fn path(repo: &Path) -> std::path::PathBuf {
        repo.join(".cih").join("feature-overrides.json")
    }

    /// Write pretty JSON to `<repo>/.cih/feature-overrides.json`, creating `.cih` if needed.
    pub fn save(&self, repo: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let path = Self::path(repo);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Add or update the assignment for `node_id`. `feature` is always set; `reason` is applied
    /// only when non-empty (so an update without a reason keeps the previous one). Returns `true`
    /// when an existing entry was updated, `false` when a new one was appended.
    pub fn upsert(&mut self, node_id: String, feature: String, reason: String) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.node_id == node_id) {
            entry.feature = feature;
            if !reason.is_empty() {
                entry.reason = reason;
            }
            true
        } else {
            self.entries.push(FeatureOverrideEntry {
                node_id,
                feature,
                reason,
            });
            false
        }
    }
}

/// Inject overrides into `entries`: remove any existing assignment for each overridden
/// node, then add a new entry with `strategy:"override"`, `confidence:1.0`, `pinned:true`.
/// Overrides are idempotent and stable across re-runs.
pub fn apply_overrides(
    mut entries: Vec<FeatureGroupEntry>,
    overrides: &FeatureOverrides,
) -> Vec<FeatureGroupEntry> {
    for ov in &overrides.entries {
        entries.retain(|e| e.node_id != ov.node_id);
        entries.push(FeatureGroupEntry {
            id: format!("feature:{}", ov.feature),
            name: ov.feature.clone(),
            node_id: ov.node_id.clone(),
            strategy: "override".to_string(),
            confidence: 1.0,
            pinned: true,
            evidence: if ov.reason.is_empty() {
                "manual override".to_string()
            } else {
                ov.reason.clone()
            },
            node_content_hash: 0,
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_adds_then_updates() {
        let mut ov = FeatureOverrides::default();
        assert!(!ov.upsert("Class:a.B".into(), "payment".into(), "r1".into())); // added
        assert_eq!(ov.entries.len(), 1);
        assert!(ov.upsert("Class:a.B".into(), "order".into(), "r2".into())); // updated
        assert_eq!(ov.entries.len(), 1);
        assert_eq!(ov.entries[0].feature, "order");
        assert_eq!(ov.entries[0].reason, "r2");
        // Empty reason on update keeps the prior reason.
        ov.upsert("Class:a.B".into(), "inventory".into(), String::new());
        assert_eq!(ov.entries[0].feature, "inventory");
        assert_eq!(ov.entries[0].reason, "r2");
    }

    #[test]
    fn apply_overrides_pins_and_replaces() {
        let base = vec![FeatureGroupEntry {
            id: "feature:auth".into(),
            name: "auth".into(),
            node_id: "Method:a.B#f/0".into(),
            strategy: "embed".into(),
            confidence: 0.4,
            pinned: false,
            evidence: "x".into(),
            node_content_hash: 7,
        }];
        let ov = FeatureOverrides {
            version: 1,
            entries: vec![FeatureOverrideEntry {
                node_id: "Method:a.B#f/0".into(),
                feature: "inventory".into(),
                reason: "llm-review: pkg match".into(),
            }],
        };
        let out = apply_overrides(base, &ov);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "inventory");
        assert!(out[0].pinned);
        assert_eq!(out[0].strategy, "override");
        assert_eq!(out[0].evidence, "llm-review: pkg match");
    }
}
