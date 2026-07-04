use super::*;

#[test]
fn cosine_identical_vectors() {
    let v = vec![1.0f32, 0.0, 0.0];
    assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
}

#[test]
fn cosine_orthogonal() {
    let a = vec![1.0f32, 0.0];
    let b = vec![0.0f32, 1.0];
    assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
}

#[test]
fn cosine_zero_vector() {
    let a = vec![0.0f32, 0.0];
    let b = vec![1.0f32, 0.0];
    assert_eq!(cosine_similarity(&a, &b), 0.0);
}

struct MockEmbedder {
    dim: usize,
}
impl Embedder for MockEmbedder {
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dim];
                let idx = t.chars().next().map(|c| c as usize % self.dim).unwrap_or(0);
                v[idx] = 1.0;
                v
            })
            .collect())
    }
}

#[test]
fn embed_strategy_assigns_residuals_above_threshold() {
    use cih_core::{Node, NodeId, NodeKind, Range};

    fn make_node(id: &str, name: &str, file: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Class,
            name: name.to_string(),
            qualified_name: None,
            file: file.to_string(),
            range: Range::default(),
            props: None,
        }
    }

    let nodes = vec![
        make_node(
            "Class:com.example.PaymentService",
            "PaymentService",
            "payment-service/src/main/java/com/example/PaymentService.java",
        ),
        make_node(
            "Class:com.example.PaymentRepo",
            "PaymentRepo",
            "payment-service/src/main/java/com/example/PaymentRepo.java",
        ),
        make_node(
            "Class:com.example.custom.PayProcessor",
            "PayProcessor",
            "custom-impl/src/main/java/com/example/PayProcessor.java",
        ),
    ];

    let prior = vec![
        FeatureGroupEntry {
            id: "feature:payment".into(),
            name: "payment".into(),
            node_id: "Class:com.example.PaymentService".into(),
            strategy: "package".into(),
            confidence: 1.0,
            pinned: false,
            evidence: String::new(),
            node_content_hash: 0,
        },
        FeatureGroupEntry {
            id: "feature:payment".into(),
            name: "payment".into(),
            node_id: "Class:com.example.PaymentRepo".into(),
            strategy: "package".into(),
            confidence: 1.0,
            pinned: false,
            evidence: String::new(),
            node_content_hash: 0,
        },
        FeatureGroupEntry {
            id: "feature:shared".into(),
            name: "shared".into(),
            node_id: "Class:com.example.custom.PayProcessor".into(),
            strategy: "package".into(),
            confidence: 1.0,
            pinned: false,
            evidence: String::new(),
            node_content_hash: 0,
        },
    ];

    let cfg = EmbedConfig {
        similarity_threshold: 0.99,
        ..EmbedConfig::default()
    };
    let strategy = EmbedStrategy::new(Arc::new(MockEmbedder { dim: 128 }), cfg);
    let input = StrategyInput {
        nodes: &nodes,
        edges: &[],
        graph_version: "v1",
        prior_assignments: &prior,
    };
    let entries = strategy.assign(&input);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "payment");
    assert_eq!(entries[0].node_id, "Class:com.example.custom.PayProcessor");
    assert_eq!(entries[0].strategy, "embed");
}
