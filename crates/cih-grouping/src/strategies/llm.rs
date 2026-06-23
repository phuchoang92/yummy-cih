use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cih_core::Node;

use crate::entry::{fnv64_node, FeatureGroupEntry};
use crate::strategy::{FeatureStrategy, StrategyInput};

/// Injected LLM caller — keeps `cih-grouping` free of HTTP/provider dependencies.
/// Implemented in `cih-engine` using `LlmAdapter`.
pub trait FeatureLlmCaller: Send + Sync {
    fn classify_batch(&self, system: &str, user: &str) -> anyhow::Result<String>;
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// Nodes per LLM call (15–20 recommended to stay within token limits).
    pub batch_size: usize,
    pub catch_all_features: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            batch_size: 18,
            catch_all_features: vec!["shared".into(), "core".into(), "common".into()],
        }
    }
}

pub struct LlmStrategy {
    caller: Arc<dyn FeatureLlmCaller>,
    config: LlmConfig,
    /// Assignments from a prior run's artifact file, used for incremental cache.
    prior_artifact: Vec<FeatureGroupEntry>,
}

impl LlmStrategy {
    pub fn new(
        caller: Arc<dyn FeatureLlmCaller>,
        config: LlmConfig,
        prior_artifact: Vec<FeatureGroupEntry>,
    ) -> Self {
        Self { caller, config, prior_artifact }
    }
}

impl FeatureStrategy for LlmStrategy {
    fn name(&self) -> &str {
        "llm"
    }

    fn feature_of(&self, _file: &str) -> String {
        self.config.catch_all_features.first().cloned().unwrap_or_else(|| "shared".into())
    }

    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        let catch_all: HashSet<&str> =
            self.config.catch_all_features.iter().map(|s| s.as_str()).collect();

        // Build incremental cache from prior run artifact (only llm-strategy entries).
        let prior_cache: HashMap<&str, &FeatureGroupEntry> = self
            .prior_artifact
            .iter()
            .filter(|e| e.strategy == "llm")
            .map(|e| (e.node_id.as_str(), e))
            .collect();

        // Collect candidate features for the vocabulary (from prior pipeline assignments).
        let feature_vocab: Vec<String> = {
            let mut seen = HashSet::new();
            let mut v: Vec<String> = input
                .prior_assignments
                .iter()
                .map(|e| e.name.clone())
                .filter(|f| seen.insert(f.clone()))
                .collect();
            v.sort();
            v
        };

        // Residuals: nodes not yet assigned to a non-catch-all feature.
        let residuals: Vec<&Node> = input
            .nodes
            .iter()
            .filter(|n| {
                let prior = input
                    .prior_assignments
                    .iter()
                    .find(|e| e.node_id == n.id.as_str());
                match prior {
                    Some(e) if !catch_all.contains(e.name.as_str()) => false,
                    _ => true,
                }
            })
            .collect();

        if residuals.is_empty() {
            return vec![];
        }

        let system_prompt = build_system_prompt(&feature_vocab);
        let mut results: Vec<FeatureGroupEntry> = Vec::with_capacity(residuals.len());

        for batch in residuals.chunks(self.config.batch_size) {
            // Separate cached (hash match) from uncached nodes.
            let mut cached_entries: Vec<FeatureGroupEntry> = Vec::new();
            let mut uncached: Vec<&Node> = Vec::new();

            for n in batch {
                let h = fnv64_node(n);
                match prior_cache.get(n.id.as_str()) {
                    Some(cached) if cached.node_content_hash == h => {
                        cached_entries.push((*cached).clone());
                    }
                    _ => uncached.push(n),
                }
            }

            results.extend(cached_entries);

            if uncached.is_empty() {
                continue;
            }

            let user_prompt = build_user_prompt(uncached.iter().copied(), &feature_vocab);
            match self.caller.classify_batch(&system_prompt, &user_prompt) {
                Ok(raw) => {
                    let parsed = parse_response(&raw);
                    for n in &uncached {
                        let h = fnv64_node(n);
                        let entry = parsed.get(n.id.as_str()).cloned().unwrap_or_else(|| {
                            ParsedEntry {
                                feature: self
                                    .config
                                    .catch_all_features
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| "shared".into()),
                                confidence: "low".into(),
                                reason: "no_llm_response".into(),
                            }
                        });
                        results.push(make_entry(n, &entry, h));
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "LLM classify_batch failed — using catch-all");
                    let fallback = self
                        .config
                        .catch_all_features
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "shared".into());
                    for n in &uncached {
                        let h = fnv64_node(n);
                        results.push(FeatureGroupEntry {
                            id: format!("feature:{fallback}"),
                            name: fallback.clone(),
                            node_id: n.id.as_str().to_string(),
                            strategy: "llm".into(),
                            confidence: 0.4,
                            pinned: false,
                            evidence: format!("llm_error:{}", truncate_error(&err.to_string())),
                            node_content_hash: h,
                        });
                    }
                }
            }
        }

        results
    }
}

fn build_system_prompt(vocab: &[String]) -> String {
    let features_section = if vocab.is_empty() {
        "  (no prior features — invent appropriate kebab-case feature slugs)".to_string()
    } else {
        vocab.iter().map(|f| format!("  - {f}")).collect::<Vec<_>>().join("\n")
    };
    format!(
        r#"You are a Java/Spring codebase classifier. Assign each class to exactly one business feature.

Known features (prefer existing ones; introduce a new slug only when none fit):
{features_section}

Output format — one JSON object per line, no extra text:
{{"id":"<class_fqn>","feature":"<slug>","confidence":"high|medium|low","reason":"<≤8 words>"}}

Rules:
- feature slug: lowercase, hyphen-separated (e.g. "payment-processing", "order-management")
- confidence: "high" = package/name clearly indicates the feature; "medium" = plausible; "low" = guessing
- reason: ≤8 words, no punctuation other than hyphens
- Emit exactly one JSON line per class. No markdown fences, no commentary."#
    )
}

fn build_user_prompt<'a>(nodes: impl Iterator<Item = &'a Node>, _vocab: &[String]) -> String {
    let mut lines = vec!["Classify these classes:".to_string(), String::new()];
    for node in nodes {
        let short_name = node.id.as_str().rsplit('.').next().unwrap_or(node.id.as_str());
        lines.push(format!("id: {}", node.id.as_str()));
        lines.push(format!("  name: {short_name}"));
        lines.push(format!("  file: {}", node.file));
        lines.push(String::new());
    }
    lines.join("\n")
}

#[derive(Debug, Clone)]
struct ParsedEntry {
    feature: String,
    confidence: String,
    reason: String,
}

fn parse_response(raw: &str) -> HashMap<String, ParsedEntry> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = val.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let feature = val
            .get("feature")
            .and_then(|v| v.as_str())
            .unwrap_or("shared")
            .to_string();
        let confidence = val
            .get("confidence")
            .and_then(|v| v.as_str())
            .unwrap_or("low")
            .to_string();
        let reason = val
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        map.insert(id.to_string(), ParsedEntry { feature, confidence, reason });
    }
    map
}

fn confidence_score(level: &str) -> f32 {
    match level {
        "high" => 0.9,
        "medium" => 0.7,
        _ => 0.4,
    }
}

fn make_entry(node: &Node, parsed: &ParsedEntry, hash: u64) -> FeatureGroupEntry {
    FeatureGroupEntry {
        id: format!("feature:{}", parsed.feature),
        name: parsed.feature.clone(),
        node_id: node.id.as_str().to_string(),
        strategy: "llm".into(),
        confidence: confidence_score(&parsed.confidence),
        pinned: false,
        evidence: format!("llm:{}", parsed.reason),
        node_content_hash: hash,
    }
}

fn truncate_error(msg: &str) -> &str {
    &msg[..msg.len().min(80)]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoLlmCaller;

    impl FeatureLlmCaller for EchoLlmCaller {
        fn classify_batch(&self, _system: &str, user: &str) -> anyhow::Result<String> {
            // Extract IDs from the user prompt and echo back classifications
            let mut lines = Vec::new();
            for line in user.lines() {
                let line = line.trim();
                if let Some(id) = line.strip_prefix("id: ") {
                    lines.push(format!(
                        r#"{{"id":"{id}","feature":"order","confidence":"high","reason":"order in class name"}}"#
                    ));
                }
            }
            Ok(lines.join("\n"))
        }
    }

    fn make_node(id: &str, file: &str) -> cih_core::Node {
        cih_core::Node {
            id: cih_core::NodeId::new(id),
            name: id.rsplit('.').next().unwrap_or(id).to_string(),
            kind: cih_core::NodeKind::Class,
            qualified_name: None,
            file: file.to_string(),
            range: cih_core::Range::default(),
            props: None,
        }
    }

    #[test]
    fn test_assign_residuals_classified() {
        let caller = Arc::new(EchoLlmCaller);
        let strategy = LlmStrategy::new(caller, LlmConfig::default(), vec![]);
        let node = make_node(
            "com.example.order.OrderService",
            "src/main/java/com/example/order/OrderService.java",
        );
        let nodes = vec![node];
        let edges = vec![];
        let input = StrategyInput {
            nodes: &nodes,
            edges: &edges,
            graph_version: "v1",
            prior_assignments: &[],
        };
        let result = strategy.assign(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "order");
        assert_eq!(result[0].strategy, "llm");
        assert!((result[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn test_non_catch_all_prior_skipped() {
        let caller = Arc::new(EchoLlmCaller);
        let strategy = LlmStrategy::new(caller, LlmConfig::default(), vec![]);
        let node = make_node(
            "com.example.payment.PaymentService",
            "src/main/java/com/example/payment/PaymentService.java",
        );
        let nodes = vec![node.clone()];
        let edges = vec![];
        // prior_assignments already has a non-catch-all assignment for this node
        let prior = FeatureGroupEntry {
            id: "feature:payment".into(),
            name: "payment".into(),
            node_id: node.id.as_str().to_string(),
            strategy: "package".into(),
            confidence: 1.0,
            pinned: false,
            evidence: "prior".into(),
            node_content_hash: 0,
        };
        let input = StrategyInput {
            nodes: &nodes,
            edges: &edges,
            graph_version: "v1",
            prior_assignments: &[prior],
        };
        let result = strategy.assign(&input);
        // Node already assigned to "payment" (not a catch-all) — LLM should skip it
        assert!(result.is_empty());
    }

    #[test]
    fn test_incremental_cache_hit() {
        let caller = Arc::new(EchoLlmCaller);
        let node = make_node(
            "com.example.cart.CartService",
            "src/main/java/com/example/cart/CartService.java",
        );
        let hash = fnv64_node(&node);
        let cached = FeatureGroupEntry {
            id: "feature:cart".into(),
            name: "cart".into(),
            node_id: node.id.as_str().to_string(),
            strategy: "llm".into(),
            confidence: 0.9,
            pinned: false,
            evidence: "llm:cart service class".into(),
            node_content_hash: hash,
        };
        let strategy = LlmStrategy::new(caller, LlmConfig::default(), vec![cached.clone()]);
        let nodes = vec![node];
        let edges = vec![];
        let input = StrategyInput {
            nodes: &nodes,
            edges: &edges,
            graph_version: "v1",
            prior_assignments: &[],
        };
        let result = strategy.assign(&input);
        assert_eq!(result.len(), 1);
        // Should return the cached entry, not call LLM
        assert_eq!(result[0].name, "cart");
        assert_eq!(result[0].evidence, "llm:cart service class");
    }

    #[test]
    fn test_parse_response_jsonl() {
        let raw = r#"
{"id":"com.example.A","feature":"order","confidence":"high","reason":"order prefix"}
not-json-skip-me
{"id":"com.example.B","feature":"payment","confidence":"medium","reason":"payment related"}
"#;
        let parsed = parse_response(raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["com.example.A"].feature, "order");
        assert_eq!(parsed["com.example.B"].confidence, "medium");
    }
}
