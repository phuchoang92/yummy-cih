use std::collections::{HashMap, HashSet};

use cih_core::{EdgeKind, NodeKind};

use crate::entry::{fnv64_node, FeatureGroupEntry};
use crate::strategy::{FeatureStrategy, StrategyInput};

/// Rule-based cross-cutting detector. Marks a node as `feature:shared` when it
/// exhibits two or more cross-cutting signals. Runs cheaply — no API calls.
///
/// Signals:
/// 1. Simple name contains a cross-cutting keyword (Filter, Interceptor, Aspect, …)
/// 2. File path contains an infrastructure path segment (common, platform, framework, …)
/// 3. Call-graph in-degree spans ≥ N distinct prior-phase features
pub struct StructuralStrategy {
    config: StructuralConfig,
}

pub struct StructuralConfig {
    /// Substrings to look for in simple class names (case-insensitive).
    pub name_keywords: Vec<String>,
    /// Path fragments that indicate cross-cutting infrastructure (case-insensitive).
    pub path_fragments: Vec<String>,
    /// Min distinct features among callers to trigger the in-degree signal.
    pub min_caller_features: usize,
    /// Min signals that must fire for a node to be labelled "shared". Default: 2.
    pub min_signals: usize,
    /// Feature names considered "unassigned" (not meaningful for in-degree counting).
    pub catch_all_features: Vec<String>,
}

impl Default for StructuralConfig {
    fn default() -> Self {
        Self {
            name_keywords: vec![
                "filter".into(),
                "interceptor".into(),
                "listener".into(),
                "audit".into(),
                "logger".into(),
                "publisher".into(),
                "security".into(),
                "aspect".into(),
                "handler".into(),
                "middleware".into(),
                "util".into(),
                "utils".into(),
                "helper".into(),
                "helpers".into(),
            ],
            path_fragments: vec![
                "platform-core".into(),
                "common".into(),
                "infrastructure".into(),
                "framework".into(),
                "platform".into(),
                "crosscutting".into(),
                "cross-cutting".into(),
                "shared".into(),
                "util/".into(),
                "utils/".into(),
            ],
            min_caller_features: 3,
            min_signals: 2,
            catch_all_features: vec![
                "shared".into(),
                "core".into(),
                "common".into(),
                "base".into(),
                "generic".into(),
            ],
        }
    }
}

impl StructuralStrategy {
    pub fn new(config: StructuralConfig) -> Self {
        Self { config }
    }

    fn name_signal(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.config
            .name_keywords
            .iter()
            .any(|kw| lower.contains(kw.as_str()))
    }

    fn path_signal(&self, file: &str) -> bool {
        let lower = file.to_lowercase();
        self.config
            .path_fragments
            .iter()
            .any(|frag| lower.contains(frag.as_str()))
    }

    fn stereotype_signal(node: &cih_core::Node) -> bool {
        node.props
            .as_ref()
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str())
            .map(|s| matches!(s, "aspect" | "filter"))
            .unwrap_or(false)
    }

    fn indegree_signal(
        &self,
        node_id: &str,
        calls_in: &HashMap<String, Vec<String>>,
        node_to_feature: &HashMap<String, String>,
    ) -> bool {
        let callers = match calls_in.get(node_id) {
            Some(c) => c,
            None => return false,
        };
        let features: HashSet<&str> = callers
            .iter()
            .filter_map(|id| node_to_feature.get(id.as_str()))
            .filter(|f| !self.is_catch_all(f))
            .map(|f| f.as_str())
            .collect();
        features.len() >= self.config.min_caller_features
    }

    fn is_catch_all(&self, feature: &str) -> bool {
        self.config.catch_all_features.iter().any(|c| c == feature)
    }
}

impl FeatureStrategy for StructuralStrategy {
    fn name(&self) -> &str {
        "structural"
    }

    fn feature_of(&self, file: &str) -> String {
        if self.path_signal(file) {
            "shared".to_string()
        } else {
            // Can't determine without name/graph context — return a placeholder.
            String::new()
        }
    }

    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        // Build calls_in: node_id → [caller_node_ids] from Calls edges
        let mut calls_in: HashMap<String, Vec<String>> = HashMap::new();
        for e in input.edges {
            if e.kind == EdgeKind::Calls {
                calls_in
                    .entry(e.dst.as_str().to_string())
                    .or_default()
                    .push(e.src.as_str().to_string());
            }
        }

        // Build node_id → feature from prior assignments
        let node_to_feature: HashMap<String, String> = input
            .prior_assignments
            .iter()
            .map(|e| (e.node_id.clone(), e.name.clone()))
            .collect();

        let mut results = Vec::new();

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

            let mut signals: Vec<&str> = Vec::new();
            if self.name_signal(&node.name) || Self::stereotype_signal(node) {
                signals.push("name_keyword");
            }
            if self.path_signal(&node.file) {
                signals.push("path_fragment");
            }
            if self.indegree_signal(node.id.as_str(), &calls_in, &node_to_feature) {
                signals.push("indegree_span");
            }

            if signals.len() >= self.config.min_signals {
                let evidence = format!("structural[{}]", signals.join(","));
                results.push(FeatureGroupEntry {
                    id: "feature:shared".to_string(),
                    name: "shared".to_string(),
                    node_id: node.id.as_str().to_string(),
                    strategy: "structural".to_string(),
                    confidence: 1.0,
                    pinned: false,
                    evidence,
                    node_content_hash: fnv64_node(node),
                });
            }
        }

        results
    }
}
