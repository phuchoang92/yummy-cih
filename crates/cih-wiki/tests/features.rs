use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::features::*;
use cih_wiki::graph::WikiGraph;

fn method_node(id: &str, file: &str) -> Node {
    let name = id
        .split('#')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("m")
        .to_string();
    Node {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Method,
        name,
        qualified_name: None,
        file: file.to_string(),
        range: Range::default(),
        props: None,
    }
}

fn comm_node(id: &str, name: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Community,
        name: name.to_string(),
        qualified_name: None,
        file: String::new(),
        range: Range::default(),
        props: None,
    }
}

fn member_edge(method: &str, comm: &str) -> Edge {
    Edge {
        src: NodeId::new(method.to_string()),
        dst: NodeId::new(comm.to_string()),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }
}

#[test]
fn feature_inferred_from_modules_path() {
    let m = method_node(
        "Method:org.phuc.commerce.modules.payment.PaymentController#handleReturn/4",
        "src/main/java/org/phuc/commerce/modules/payment/PaymentController.java",
    );
    let comm = comm_node("Community:0", "Payment");
    let g = WikiGraph::build(
        std::slice::from_ref(&m),
        &[],
        &[comm],
        &[member_edge(m.id.as_str(), "Community:0")],
    );
    assert_eq!(infer_community_feature("Community:0", &g), "payment");
}

#[test]
fn feature_falls_back_to_shared() {
    let m = method_node("Method:com.example.Foo#bar/0", "Test.java");
    let comm = comm_node("Community:0", "misc");
    let g = WikiGraph::build(
        std::slice::from_ref(&m),
        &[],
        &[comm],
        &[member_edge(m.id.as_str(), "Community:0")],
    );
    assert_eq!(infer_community_feature("Community:0", &g), "shared");
}

#[test]
fn feature_falls_back_to_route_segment() {
    let m = method_node("Method:com.example.Foo#bar/0", "Test.java");
    let route = Node {
        id: NodeId::new("Route:GET /api/v1/orders/{id}".to_string()),
        kind: NodeKind::Route,
        name: "GET /api/v1/orders/{id}".to_string(),
        qualified_name: None,
        file: "Test.java".to_string(),
        range: Range::default(),
        props: Some(serde_json::json!({
            "httpMethod": "GET",
            "path": "/api/v1/orders/{id}"
        })),
    };
    let comm = comm_node("Community:0", "misc");
    let route_edge = Edge {
        src: m.id.clone(),
        dst: route.id.clone(),
        kind: EdgeKind::HandlesRoute,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    };
    let g = WikiGraph::build(
        &[m.clone(), route],
        &[route_edge],
        &[comm],
        &[member_edge(m.id.as_str(), "Community:0")],
    );
    assert_eq!(infer_community_feature("Community:0", &g), "orders");
}

#[test]
fn dev_slug_uses_primary_class_name() {
    let m = method_node(
        "Method:org.phuc.commerce.modules.payment.PaymentController#handleReturn/4",
        "src/main/java/org/phuc/commerce/modules/payment/PaymentController.java",
    );
    let comm = comm_node("Community:0", "Payment");
    let g = WikiGraph::build(
        std::slice::from_ref(&m),
        &[],
        &[comm],
        &[member_edge(m.id.as_str(), "Community:0")],
    );
    let groups = group_communities_by_feature(&g);
    let paths = build_dev_page_paths(&groups, &g);
    assert_eq!(paths["Community:0"], "payment/dev/payment-controller");
}

#[test]
fn slug_collision_gets_suffix() {
    let m1 = method_node(
        "Method:com.example.modules.order.OrderService#save/0",
        "src/main/java/com/example/modules/order/OrderService.java",
    );
    let m2 = method_node(
        "Method:com.example.modules.order.OrderService#find/0",
        "src/main/java/com/example/modules/order/OrderService.java",
    );
    let c1 = comm_node("Community:1", "Order");
    let c2 = comm_node("Community:2", "Order");
    let g = WikiGraph::build(
        &[m1.clone(), m2.clone()],
        &[],
        &[c1, c2],
        &[
            member_edge(m1.id.as_str(), "Community:1"),
            member_edge(m2.id.as_str(), "Community:2"),
        ],
    );
    let groups = group_communities_by_feature(&g);
    let paths = build_dev_page_paths(&groups, &g);
    let p1 = paths.get("Community:1").unwrap();
    let p2 = paths.get("Community:2").unwrap();
    assert_ne!(p1, p2, "paths must differ");
    assert!(
        p1 == "order/dev/order-service" || p2 == "order/dev/order-service",
        "one must have clean slug"
    );
}

#[test]
fn pascal_to_kebab_converts_correctly() {
    assert_eq!(pascal_to_kebab("PaymentController"), "payment-controller");
    assert_eq!(
        pascal_to_kebab("PaymentOrchestrationService"),
        "payment-orchestration-service"
    );
    assert_eq!(pascal_to_kebab("PosOrderService"), "pos-order-service");
    // Acronyms must stay together
    assert_eq!(
        pascal_to_kebab("ProgressiveEMICalculator"),
        "progressive-emi-calculator"
    );
    assert_eq!(pascal_to_kebab("URLParser"), "url-parser");
    assert_eq!(
        pascal_to_kebab("LoanReadPlatformServiceImpl"),
        "loan-read-platform-service-impl"
    );
}
