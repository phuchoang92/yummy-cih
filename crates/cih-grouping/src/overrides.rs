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
