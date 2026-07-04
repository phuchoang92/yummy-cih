use cih_core::{Edge, Node};

use crate::entry::FeatureGroupEntry;

pub struct StrategyInput<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
    pub graph_version: &'a str,
    /// Assignments from earlier strategies in a hybrid pipeline.
    /// Empty when this is the first (or only) strategy running.
    pub prior_assignments: &'a [FeatureGroupEntry],
}

/// Compute dense vector embeddings for a batch of texts.
/// Implemented in `cih-engine` using `cih-embed::EmbedModel`; injected into
/// `EmbedStrategy` so that `cih-grouping` stays free of heavy ML dependencies.
pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

pub trait FeatureStrategy: Send + Sync {
    fn name(&self) -> &str;

    /// Classify a single file path to a feature slug.
    /// Used inline by the wiki builder for per-node queries without full graph context.
    fn feature_of(&self, file: &str) -> String;

    /// Classify all nodes at once. Default impl calls `feature_of` per node.
    /// Override for strategies that benefit from batch context (structural, embed, LLM).
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
