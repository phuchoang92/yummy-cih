use serde::{Deserialize, Serialize};

/// One node-to-feature assignment record, written to `groups.jsonl`.
/// Lives in `cih-grouping`; re-read by `cih-wiki` in Phase 2+.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureGroupEntry {
    /// "feature:<slug>" e.g. "feature:overdraft"
    pub id: String,
    /// slug e.g. "overdraft"
    pub name: String,
    pub node_id: String,
    /// "package" | "llm" | "embed" | "structural" | "override"
    pub strategy: String,
    /// 0.0–1.0
    pub confidence: f32,
    /// true when locked by a human override
    pub pinned: bool,
    /// human-readable reason, e.g. "module dir banking-overdraft stripped prefix+suffix"
    pub evidence: String,
    /// FNV-64 of (fqn|file_path|kind) for cache hits
    pub node_content_hash: u64,
}
