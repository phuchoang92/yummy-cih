use std::collections::HashMap;

use super::*;
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

fn meta(kind: &str, name: &str, file: &str) -> NodeMeta {
    NodeMeta {
        kind: kind.to_string(),
        name: name.to_string(),
        file: file.to_string(),
    }
}

#[test]
fn slugify_normalizes() {
    assert_eq!(slugify("banking-overdraft"), "banking-overdraft");
    assert_eq!(slugify("PaymentService"), "paymentservice");
    assert_eq!(slugify("  Foo__Bar  "), "foo-bar");
    assert_eq!(slugify("---"), "");
}

#[test]
fn strip_suffixes_handles_stacked() {
    assert_eq!(strip_suffixes("PaymentServiceImpl"), "Payment");
    assert_eq!(strip_suffixes("OrderController"), "Order");
    assert_eq!(strip_suffixes("Overdraft"), "Overdraft");
}

#[test]
fn derive_slug_prefers_feature_container_segment() {
    // The `modules/<feature>/` convention wins over class-name stripping.
    let slug = derive_slug(
        "Class:org.phuc.commerce.modules.product.services.CategoryService",
        "src/main/java/org/phuc/commerce/modules/product/services/CategoryService.java",
        "CategoryService",
        0,
    );
    assert_eq!(slug, "product");
    // `domain/<feature>/` too.
    let slug = derive_slug(
        "Class:com.acme.domain.billing.Invoice",
        "src/main/java/com/acme/domain/billing/Invoice.java",
        "Invoice",
        1,
    );
    assert_eq!(slug, "billing");
}

#[test]
fn derive_slug_prefers_module_dir() {
    let slug = derive_slug(
        "Class:com.bank.OverdraftService",
        "banking-overdraft/src/main/java/com/bank/OverdraftService.java",
        "OverdraftService",
        0,
    );
    assert_eq!(slug, "banking-overdraft");
}

#[test]
fn derive_slug_falls_back_to_class_name() {
    // No feature container / hyphenated module dir → strip class suffix.
    let slug = derive_slug(
        "Class:com.bank.PaymentService",
        "src/main/java/com/bank/PaymentService.java",
        "PaymentService",
        3,
    );
    assert_eq!(slug, "payment");
}

#[test]
fn derive_slug_uses_owner_class_for_member_labels() {
    // A method label with a generic simple name ("list") derives from its owner class instead.
    let slug = derive_slug(
        "Method:com.bank.OrderProcessor#list/0",
        "src/main/java/com/bank/OrderProcessor.java",
        "list",
        4,
    );
    assert_eq!(slug, "orderprocessor");
    // A method whose owner is a *Service falls through the stripped suffix cleanly.
    let slug = derive_slug(
        "Method:com.bank.PaymentService#getName/0",
        "src/main/java/com/bank/PaymentService.java",
        "getName",
        5,
    );
    assert_eq!(slug, "payment");
}

#[test]
fn derive_slug_uses_package_then_cluster_id() {
    // Generic class name + no module dir → immediate package dir.
    let slug = derive_slug(
        "Class:com.bank.billing.Impl",
        "src/main/java/com/bank/billing/Impl.java",
        "Impl",
        7,
    );
    assert_eq!(slug, "billing");
    // Nothing usable at all → cluster id.
    let slug = derive_slug("Class:Service", "Service.java", "Service", 9);
    assert_eq!(slug, "cluster-9");
}

#[test]
fn generic_tokens_are_blocklisted() {
    assert!(is_generic_segment("list"));
    assert!(is_generic_segment("name"));
    assert!(is_generic_segment("is"));
    assert!(is_generic_segment("dto"));
    assert!(!is_generic_segment("product"));
    assert!(!is_generic_segment("payment"));
}

#[test]
fn assign_emits_slugs_and_shared() {
    let nodes = vec![
        make_node(
            "Class:com.bank.PaymentService",
            "PaymentService",
            "payments-svc/src/main/java/com/bank/PaymentService.java",
        ),
        make_node(
            "Class:com.bank.BillingController",
            "BillingController",
            "payments-svc/src/main/java/com/bank/BillingController.java",
        ),
        // Not in any cluster → should become "shared".
        make_node(
            "Class:com.bank.Loose",
            "Loose",
            "misc/src/main/java/com/bank/Loose.java",
        ),
    ];

    // Two clustered nodes in cluster 0; identical vectors so the centroid == them.
    let clusters = vec![
        ("Class:com.bank.PaymentService".to_string(), 0usize),
        ("Class:com.bank.BillingController".to_string(), 0usize),
    ];
    let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
    vectors.insert("Class:com.bank.PaymentService".into(), vec![1.0, 0.0, 0.0]);
    vectors.insert("Class:com.bank.BillingController".into(), vec![1.0, 0.0, 0.0]);
    let mut m: HashMap<String, NodeMeta> = HashMap::new();
    m.insert(
        "Class:com.bank.PaymentService".into(),
        meta(
            "Class",
            "PaymentService",
            "payments-svc/src/main/java/com/bank/PaymentService.java",
        ),
    );
    m.insert(
        "Class:com.bank.BillingController".into(),
        meta(
            "Class",
            "BillingController",
            "payments-svc/src/main/java/com/bank/BillingController.java",
        ),
    );

    let strategy =
        EmbedClusterStrategy::new(clusters, vectors, m, EmbedClusterConfig::default());
    let input = StrategyInput {
        nodes: &nodes,
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = strategy.assign(&input);
    assert_eq!(entries.len(), 3);

    let by_id: HashMap<&str, &FeatureGroupEntry> =
        entries.iter().map(|e| (e.node_id.as_str(), e)).collect();

    // Both clustered nodes share the same slug (hyphenated module dir "payments-svc"),
    // regardless of which tied node becomes the label.
    let ps = by_id["Class:com.bank.PaymentService"];
    let bc = by_id["Class:com.bank.BillingController"];
    assert_eq!(ps.strategy, "embed");
    assert_eq!(ps.name, "payments-svc");
    assert_eq!(bc.name, "payments-svc");
    assert!(ps.confidence > 0.99, "identical vector → sim ~1.0");

    // The loose node is unclustered → shared.
    let loose = by_id["Class:com.bank.Loose"];
    assert_eq!(loose.name, "shared");
    assert_eq!(loose.confidence, 0.0);
}

#[test]
fn colliding_slugs_are_disambiguated() {
    // Two clusters whose label nodes derive the same base slug must not collapse.
    let nodes = vec![
        make_node("Class:a.Payment", "Payment", "a/com/x/Payment.java"),
        make_node("Class:b.Payment", "Payment", "b/com/y/Payment.java"),
    ];
    let clusters = vec![
        ("Class:a.Payment".to_string(), 0usize),
        ("Class:b.Payment".to_string(), 1usize),
    ];
    let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
    vectors.insert("Class:a.Payment".into(), vec![1.0, 0.0]);
    vectors.insert("Class:b.Payment".into(), vec![0.0, 1.0]);
    let mut m: HashMap<String, NodeMeta> = HashMap::new();
    m.insert(
        "Class:a.Payment".into(),
        meta("Class", "Payment", "a/com/x/Payment.java"),
    );
    m.insert(
        "Class:b.Payment".into(),
        meta("Class", "Payment", "b/com/y/Payment.java"),
    );

    let strategy =
        EmbedClusterStrategy::new(clusters, vectors, m, EmbedClusterConfig::default());
    let input = StrategyInput {
        nodes: &nodes,
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = strategy.assign(&input);
    let names: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.name.as_str()).collect();
    // "payment" and "payment-1" — distinct.
    assert_eq!(names.len(), 2, "slugs must be disambiguated: {names:?}");
    assert!(names.contains("payment"));
    assert!(names.contains("payment-1"));
}
