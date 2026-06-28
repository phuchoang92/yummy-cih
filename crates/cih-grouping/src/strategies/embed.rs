use std::collections::HashMap;
use std::sync::Arc;

use cih_core::{EdgeKind, NodeKind};

use crate::entry::{fnv64_node, FeatureGroupEntry};
use crate::strategy::{Embedder, FeatureStrategy, StrategyInput};

/// Configuration for the embedding-based feature classifier.
pub struct EmbedConfig {
    /// Cosine similarity threshold for accepting an assignment. Default: 0.65.
    pub similarity_threshold: f32,
    /// Feature names considered residuals (no meaningful assignment from prior strategies).
    pub catch_all_features: Vec<String>,
    /// Maximum number of residual nodes to embed in one run (safety cap).
    pub max_residuals: usize,
    /// Maximum nodes per feature cluster used to compute centroids.
    pub max_cluster_nodes: usize,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.65,
            catch_all_features: vec!["shared".into()],
            max_residuals: 2_000,
            max_cluster_nodes: 200,
        }
    }
}

/// Assigns residual nodes (those still mapped to a catch-all feature by prior strategies)
/// by computing per-feature centroid embeddings and using cosine similarity.
///
/// `Embedder` is injected so that `cih-grouping` does not depend on `fastembed` directly.
pub struct EmbedStrategy {
    embedder: Arc<dyn Embedder>,
    config: EmbedConfig,
}

impl EmbedStrategy {
    pub fn new(embedder: Arc<dyn Embedder>, config: EmbedConfig) -> Self {
        Self { embedder, config }
    }

    fn is_catch_all(&self, feature: &str) -> bool {
        self.config
            .catch_all_features
            .iter()
            .any(|c| c == feature)
    }
}

impl FeatureStrategy for EmbedStrategy {
    fn name(&self) -> &str {
        "embed"
    }

    fn feature_of(&self, _file: &str) -> String {
        // Cannot do meaningful single-file embedding without cluster context.
        "shared".to_string()
    }

    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        // Build a node_id → feature map from prior assignments
        let prior_map: HashMap<&str, &str> = input
            .prior_assignments
            .iter()
            .map(|e| (e.node_id.as_str(), e.name.as_str()))
            .collect();

        if prior_map.is_empty() {
            tracing::debug!("EmbedStrategy: no prior assignments — skipping");
            return vec![];
        }

        // Build a quick node lookup
        let nodes_by_id: HashMap<&str, &cih_core::Node> = input
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n))
            .collect();

        // Build per-node method name list from HasMethod edges (for embedding text quality)
        let mut method_names: HashMap<&str, Vec<String>> = HashMap::new();
        for e in input.edges {
            if e.kind == EdgeKind::HasMethod {
                if let Some(method) = nodes_by_id.get(e.dst.as_str()) {
                    method_names
                        .entry(e.src.as_str())
                        .or_default()
                        .push(method.name.clone());
                }
            }
        }

        // Separate: clustered nodes (known non-catch-all features) vs residuals
        let mut clusters: HashMap<String, Vec<String>> = HashMap::new(); // feature → [node texts]
        let mut residuals: Vec<(&cih_core::Node, String)> = Vec::new(); // (node, text)

        for node in input.nodes {
            if !matches!(
                node.kind,
                NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Enum
                    | NodeKind::Record
                    | NodeKind::Annotation
            ) {
                continue;
            }
            let node_id = node.id.as_str();
            let text = build_embed_text(node, method_names.get(node_id).map(|v| v.as_slice()));

            match prior_map.get(node_id) {
                Some(&feat) if !self.is_catch_all(feat) => {
                    let cluster = clusters.entry(feat.to_string()).or_default();
                    if cluster.len() < self.config.max_cluster_nodes {
                        cluster.push(text);
                    }
                }
                _ => {
                    residuals.push((node, text));
                }
            }
        }

        let n_residuals = residuals.len().min(self.config.max_residuals);
        if n_residuals == 0 || clusters.is_empty() {
            tracing::debug!(
                residuals = residuals.len(),
                clusters = clusters.len(),
                "EmbedStrategy: nothing to do"
            );
            return vec![];
        }

        tracing::debug!(
            residuals = n_residuals,
            clusters = clusters.len(),
            "EmbedStrategy: computing centroids"
        );

        // Embed all cluster nodes and compute per-feature centroids
        let feature_names: Vec<String> = clusters.keys().cloned().collect();
        let all_cluster_texts: Vec<String> = feature_names
            .iter()
            .flat_map(|f| clusters[f].iter().cloned())
            .collect();

        let cluster_embeddings = match self.embedder.embed(&all_cluster_texts) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "EmbedStrategy: cluster embedding failed");
                return vec![];
            }
        };

        // Compute centroids
        let mut centroids: HashMap<String, Vec<f32>> = HashMap::new();
        let mut offset = 0;
        for feat in &feature_names {
            let n = clusters[feat].len();
            if n == 0 {
                offset += n;
                continue;
            }
            let slice = &cluster_embeddings[offset..offset + n];
            let dim = slice[0].len();
            let mut centroid = vec![0.0f32; dim];
            for emb in slice {
                for (i, &v) in emb.iter().enumerate() {
                    centroid[i] += v;
                }
            }
            let scale = 1.0 / n as f32;
            for v in &mut centroid {
                *v *= scale;
            }
            centroids.insert(feat.clone(), centroid);
            offset += n;
        }

        // Embed residuals
        let residual_texts: Vec<String> = residuals[..n_residuals]
            .iter()
            .map(|(_, t)| t.clone())
            .collect();
        let residual_embeddings = match self.embedder.embed(&residual_texts) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "EmbedStrategy: residual embedding failed");
                return vec![];
            }
        };

        // Assign by nearest centroid above threshold
        let mut results = Vec::new();
        for (idx, (node, _)) in residuals[..n_residuals].iter().enumerate() {
            let emb = &residual_embeddings[idx];
            let best = feature_names
                .iter()
                .filter_map(|f| {
                    centroids.get(f).map(|c| (f.as_str(), cosine_similarity(emb, c)))
                })
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            if let Some((feat, sim)) = best {
                if sim >= self.config.similarity_threshold {
                    results.push(FeatureGroupEntry {
                        id: format!("feature:{}", feat),
                        name: feat.to_string(),
                        node_id: node.id.as_str().to_string(),
                        strategy: "embed".to_string(),
                        confidence: sim,
                        pinned: false,
                        evidence: format!("cosine_similarity:{:.3}", sim),
                        node_content_hash: fnv64_node(node),
                    });
                }
            }
        }

        tracing::debug!(assigned = results.len(), "EmbedStrategy: assigned residuals");
        results
    }
}

/// Build a text string suitable for embedding from a node's metadata.
/// Format: `{name} {stereotype?} {top5_methods}` — no source body needed.
fn build_embed_text(node: &cih_core::Node, method_names: Option<&[String]>) -> String {
    let mut parts = Vec::new();
    parts.push(node.name.clone());

    if let Some(props) = &node.props {
        if let Some(s) = props.get("stereotype").and_then(|v| v.as_str()) {
            parts.push(s.to_string());
        }
    }

    if let Some(methods) = method_names {
        for m in methods.iter().take(5) {
            parts.push(m.clone());
        }
    }

    // Include the last meaningful segment of the file path for context
    if let Some(seg) = node
        .file
        .rsplit('/')
        .find(|s| !s.is_empty() && !s.ends_with(".java") && !s.ends_with(".kt"))
    {
        parts.push(seg.to_string());
    }

    parts.join(" ")
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

#[cfg(test)]
#[path = "embed_tests.rs"]
mod tests;
