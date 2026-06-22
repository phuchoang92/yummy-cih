use cih_core::Node;
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

/// FNV-1a 64-bit hash of `node_id|file|kind` — stable cache key for incremental runs.
pub fn fnv64_node(node: &Node) -> u64 {
    let key = format!("{}|{}|{:?}", node.id.as_str(), node.file, node.kind);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
