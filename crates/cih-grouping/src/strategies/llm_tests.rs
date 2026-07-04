use std::sync::Arc;

use super::*;

struct EchoLlmCaller;

impl FeatureLlmCaller for EchoLlmCaller {
    fn classify_batch(&self, _system: &str, user: &str) -> anyhow::Result<String> {
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
