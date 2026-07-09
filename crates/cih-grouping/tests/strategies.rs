use cih_core::{Node, NodeId, NodeKind, Range};
use cih_grouping::{
    FeatureStrategy, HybridStrategy, PackageConfig, PackageStrategy, StrategyInput,
    StructuralConfig, StructuralStrategy,
};

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

fn make_node_kind(id: &str, name: &str, kind: NodeKind, file: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind,
        name: name.to_string(),
        qualified_name: None,
        file: file.to_string(),
        range: Range::default(),
        props: None,
    }
}

fn default_catch_all() -> Vec<String> {
    vec!["shared".into(), "core".into(), "common".into()]
}

// ── hybrid strategy tests ─────────────────────────────────────────────────────

#[test]
fn package_non_catchall_overrides_structural_shared() {
    let s_cfg = StructuralConfig {
        min_signals: 2,
        ..Default::default()
    };
    let structural = Box::new(StructuralStrategy::new(s_cfg));
    let package = Box::new(PackageStrategy::new(PackageConfig::default()));
    let hybrid = HybridStrategy::new(vec![structural, package], default_catch_all());
    let node = make_node(
        "Class:com.example.payment.PaymentFilter",
        "PaymentFilter",
        "payment-service/src/main/java/com/example/payment/PaymentFilter.java",
    );
    let input = StrategyInput {
        nodes: &[node],
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = hybrid.assign(&input);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "payment");
}

#[test]
fn catch_all_does_not_override_domain_assignment() {
    let package = Box::new(PackageStrategy::new(PackageConfig::default()));
    let s_cfg = StructuralConfig {
        min_signals: 1,
        ..Default::default()
    };
    let structural = Box::new(StructuralStrategy::new(s_cfg));
    let hybrid = HybridStrategy::new(vec![package, structural], default_catch_all());
    let node = make_node(
        "Class:com.example.PaymentService",
        "PaymentService",
        "payment-service/src/main/java/com/example/payment/PaymentService.java",
    );
    let input = StrategyInput {
        nodes: &[node],
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = hybrid.assign(&input);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].name, "payment",
        "package domain should not be overridden"
    );
}

#[test]
fn feature_of_delegates_in_order() {
    let package = Box::new(PackageStrategy::new(PackageConfig::default()));
    let hybrid = HybridStrategy::new(vec![package], default_catch_all());
    assert_eq!(
        hybrid.feature_of("payment-service/src/main/java/com/example/PaymentService.java"),
        "payment"
    );
    assert_eq!(hybrid.feature_of("unknown.java"), "shared");
}

// ── structural strategy tests ─────────────────────────────────────────────────

#[test]
fn cross_cutting_name_and_path_triggers() {
    let s = StructuralStrategy::new(StructuralConfig::default());
    let node = make_node_kind(
        "Class:com.example.common.AuditFilter",
        "AuditFilter",
        NodeKind::Class,
        "common/src/main/java/com/example/common/AuditFilter.java",
    );
    let input = StrategyInput {
        nodes: &[node],
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = s.assign(&input);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "shared");
    assert_eq!(entries[0].strategy, "structural");
}

#[test]
fn single_signal_does_not_trigger() {
    let s = StructuralStrategy::new(StructuralConfig::default());
    let node = make_node_kind(
        "Class:com.example.payment.PaymentFilter",
        "PaymentFilter",
        NodeKind::Class,
        "payment-service/src/main/java/com/example/payment/PaymentFilter.java",
    );
    let input = StrategyInput {
        nodes: &[node],
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = s.assign(&input);
    assert!(entries.is_empty(), "single signal should not trigger");
}

#[test]
fn min_signals_1_flags_single_keyword() {
    let cfg = StructuralConfig {
        min_signals: 1,
        ..Default::default()
    };
    let s = StructuralStrategy::new(cfg);
    let node = make_node_kind(
        "Class:com.example.payment.PaymentFilter",
        "PaymentFilter",
        NodeKind::Class,
        "payment-service/src/main/java/com/example/PaymentFilter.java",
    );
    let input = StrategyInput {
        nodes: &[node],
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = s.assign(&input);
    assert_eq!(entries.len(), 1);
}
