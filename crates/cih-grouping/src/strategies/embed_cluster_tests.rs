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

    // Two clustered nodes in cluster 0; both sit at their cluster centroid (sim ~1.0).
    let clusters = vec![
        ("Class:com.bank.PaymentService".to_string(), 0usize),
        ("Class:com.bank.BillingController".to_string(), 0usize),
    ];
    let mut sims: HashMap<String, f32> = HashMap::new();
    sims.insert("Class:com.bank.PaymentService".into(), 1.0);
    sims.insert("Class:com.bank.BillingController".into(), 1.0);
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

    let strategy = EmbedClusterStrategy::new(
        clusters,
        sims,
        m,
        EmbedClusterConfig::default(),
        None,
        Vec::new(),
    );
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
fn colliding_base_disambiguated_by_subpackage_never_counter() {
    // Two clusters both under modules/product but different sub-packages (dto vs services) must get
    // meaningful names (product-dto / product-services) — NEVER a numeric counter.
    let dto = "Class:com.shop.modules.product.dto.ProductDto";
    let svc = "Class:com.shop.modules.product.services.ProductService";
    let dto_file = "src/main/java/com/shop/modules/product/dto/ProductDto.java";
    let svc_file = "src/main/java/com/shop/modules/product/services/ProductService.java";
    let nodes = vec![
        make_node(dto, "ProductDto", dto_file),
        make_node(svc, "ProductService", svc_file),
    ];
    let clusters = vec![(dto.to_string(), 0usize), (svc.to_string(), 1usize)];
    let mut sims: HashMap<String, f32> = HashMap::new();
    sims.insert(dto.into(), 1.0);
    sims.insert(svc.into(), 1.0);
    let mut m: HashMap<String, NodeMeta> = HashMap::new();
    m.insert(dto.into(), meta("Class", "ProductDto", dto_file));
    m.insert(svc.into(), meta("Class", "ProductService", svc_file));

    let strategy = EmbedClusterStrategy::new(
        clusters,
        sims,
        m,
        EmbedClusterConfig::default(),
        None,
        Vec::new(),
    );
    let input = StrategyInput {
        nodes: &nodes,
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let entries = strategy.assign(&input);
    let names: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names.len(), 2, "slugs must be disambiguated: {names:?}");
    assert!(names.contains("product-dto"), "got {names:?}");
    assert!(names.contains("product-services"), "got {names:?}");
    // Never a numeric counter.
    assert!(
        !names.iter().any(|n| n
            .rsplit('-')
            .next()
            .is_some_and(|last| last.chars().all(|c| c.is_ascii_digit()))),
        "no name should end in -<digit>: {names:?}"
    );
}

struct StubLlm {
    reply: String,
}
impl crate::strategies::llm::FeatureLlmCaller for StubLlm {
    fn classify_batch(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
        Ok(self.reply.clone())
    }
}

#[test]
fn llm_labeling_overrides_deterministic_name() {
    let dto = "Class:com.shop.modules.product.dto.ProductDto";
    let svc = "Class:com.shop.modules.product.services.ProductService";
    let dto_file = "src/main/java/com/shop/modules/product/dto/ProductDto.java";
    let svc_file = "src/main/java/com/shop/modules/product/services/ProductService.java";
    let nodes = vec![
        make_node(dto, "ProductDto", dto_file),
        make_node(svc, "ProductService", svc_file),
    ];
    let clusters = vec![(dto.to_string(), 0usize), (svc.to_string(), 1usize)];
    let mut sims: HashMap<String, f32> = HashMap::new();
    sims.insert(dto.into(), 1.0);
    sims.insert(svc.into(), 1.0);
    let mut m: HashMap<String, NodeMeta> = HashMap::new();
    m.insert(dto.into(), meta("Class", "ProductDto", dto_file));
    m.insert(svc.into(), meta("Class", "ProductService", svc_file));

    // LLM echoes the deterministic cluster anchors and renames them.
    let reply = "{\"cluster\":\"product-dto\",\"name\":\"product-catalog\"}\n\
                 {\"cluster\":\"product-services\",\"name\":\"Product Ordering\"}";
    let strategy = EmbedClusterStrategy::new(
        clusters,
        sims,
        m,
        EmbedClusterConfig::default(),
        Some(std::sync::Arc::new(StubLlm { reply: reply.into() })),
        Vec::new(),
    );
    let input = StrategyInput {
        nodes: &nodes,
        edges: &[],
        graph_version: "v1",
        prior_assignments: &[],
    };
    let names: std::collections::HashSet<String> = strategy
        .assign(&input)
        .into_iter()
        .map(|e| e.name)
        .collect();
    // LLM slug used verbatim; a non-slug reply is slugified defensively.
    assert!(names.contains("product-catalog"), "got {names:?}");
    assert!(names.contains("product-ordering"), "got {names:?}");
}

struct CountingLlm {
    reply: String,
    calls: std::sync::atomic::AtomicUsize,
}
impl crate::strategies::llm::FeatureLlmCaller for CountingLlm {
    fn classify_batch(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.reply.clone())
    }
}

fn single_product_cluster() -> (Vec<cih_core::Node>, Vec<(String, usize)>, HashMap<String, f32>, HashMap<String, NodeMeta>) {
    let id = "Class:com.shop.modules.product.dto.ProductDto";
    let file = "src/main/java/com/shop/modules/product/dto/ProductDto.java";
    let nodes = vec![make_node(id, "ProductDto", file)];
    let clusters = vec![(id.to_string(), 0usize)];
    let mut sims: HashMap<String, f32> = HashMap::new();
    sims.insert(id.into(), 1.0);
    let mut m: HashMap<String, NodeMeta> = HashMap::new();
    m.insert(id.into(), meta("Class", "ProductDto", file));
    (nodes, clusters, sims, m)
}

fn prior_entry(name: &str, node_id: &str, evidence: &str) -> FeatureGroupEntry {
    FeatureGroupEntry {
        id: format!("feature:{name}"),
        name: name.to_string(),
        node_id: node_id.to_string(),
        strategy: "embed".to_string(),
        confidence: 1.0,
        pinned: false,
        evidence: evidence.to_string(),
        node_content_hash: 0,
    }
}

#[test]
fn cache_reuses_only_llm_marked_prior() {
    let member = "Class:com.shop.modules.product.dto.ProductDto";
    let reply = "{\"cluster\":\"product\",\"name\":\"fresh-label\"}";

    // (a) Prior is LLM-marked and matches the member set → reuse it, LLM NOT called.
    let (nodes, clusters, sims, m) = single_product_cluster();
    let llm = std::sync::Arc::new(CountingLlm {
        reply: reply.into(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let strategy = EmbedClusterStrategy::new(
        clusters,
        sims,
        m,
        EmbedClusterConfig::default(),
        Some(llm.clone()),
        vec![prior_entry("product-catalog", member, "labeler=llm knn-leiden sim=1.000")],
    );
    let input = StrategyInput { nodes: &nodes, edges: &[], graph_version: "v1", prior_assignments: &[] };
    let names: std::collections::HashSet<String> =
        strategy.assign(&input).into_iter().map(|e| e.name).collect();
    assert!(names.contains("product-catalog"), "cache hit expected: {names:?}");
    assert_eq!(llm.calls.load(std::sync::atomic::Ordering::Relaxed), 0, "LLM must not be called on cache hit");

    // (b) Prior is deterministic (labeler=path) → ignored, LLM IS called (first-enable relabel).
    let (nodes_b, clusters, sims, m) = single_product_cluster();
    let llm = std::sync::Arc::new(CountingLlm {
        reply: reply.into(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let strategy = EmbedClusterStrategy::new(
        clusters,
        sims,
        m,
        EmbedClusterConfig::default(),
        Some(llm.clone()),
        vec![prior_entry("product-dto", member, "labeler=path knn-leiden sim=1.000")],
    );
    let input_b = StrategyInput { nodes: &nodes_b, edges: &[], graph_version: "v1", prior_assignments: &[] };
    let names: std::collections::HashSet<String> =
        strategy.assign(&input_b).into_iter().map(|e| e.name).collect();
    assert!(names.contains("fresh-label"), "deterministic prior must be ignored → LLM relabels: {names:?}");
    assert_eq!(llm.calls.load(std::sync::atomic::Ordering::Relaxed), 1, "LLM must be called when prior is path-labeled");
}
