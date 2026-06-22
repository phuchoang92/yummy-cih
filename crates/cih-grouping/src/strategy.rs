use cih_core::{Edge, Node};

use crate::entry::FeatureGroupEntry;

pub struct StrategyInput<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
    pub graph_version: &'a str,
}

/// Sync file-path classifier. Upgraded to async in Phase 3 when LLM/embed strategies arrive.
pub trait FeatureStrategy: Send + Sync {
    fn name(&self) -> &str;

    /// Classify a single file path to a feature slug.
    /// Used inline by the wiki builder in Phase 1.
    fn feature_of(&self, file: &str) -> String;

    /// Classify all nodes at once. Default impl calls `feature_of` per node.
    /// Override for strategies that batch (LLM, embed).
    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        input
            .nodes
            .iter()
            .map(|n| {
                let feat = self.feature_of(&n.file);
                FeatureGroupEntry {
                    id: format!("feature:{}", feat),
                    name: feat.clone(),
                    node_id: n.id.as_str().to_string(),
                    strategy: self.name().to_string(),
                    confidence: 1.0,
                    pinned: false,
                    evidence: format!("file_path:{}", n.file),
                    node_content_hash: 0,
                }
            })
            .collect()
    }
}
