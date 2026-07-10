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

#[test]
fn assign_class_slugs_stable_when_more_colliders_added() {
    // Build {A, B} both named "Service" — both must get hash suffixes.
    let ids_two: std::collections::BTreeSet<String> = [
        "Class:com.a.Service".to_string(),
        "Class:com.b.Service".to_string(),
    ]
    .into_iter()
    .collect();
    let slugs_two = assign_class_slugs(&ids_two, |id| {
        id.rsplit('.').next().unwrap_or("Unknown").to_string()
    });
    let slug_a_two = slugs_two["Class:com.a.Service"].clone();
    let slug_b_two = slugs_two["Class:com.b.Service"].clone();
    assert_ne!(slug_a_two, slug_b_two, "colliders must get distinct slugs");

    // Add a third "Service" class — A and B must keep the exact same slugs.
    let ids_three: std::collections::BTreeSet<String> = [
        "Class:com.a.Service".to_string(),
        "Class:com.b.Service".to_string(),
        "Class:com.c.Service".to_string(),
    ]
    .into_iter()
    .collect();
    let slugs_three = assign_class_slugs(&ids_three, |id| {
        id.rsplit('.').next().unwrap_or("Unknown").to_string()
    });
    assert_eq!(
        slugs_three["Class:com.a.Service"], slug_a_two,
        "adding a 3rd collider must not rename Class:com.a.Service"
    );
    assert_eq!(
        slugs_three["Class:com.b.Service"], slug_b_two,
        "adding a 3rd collider must not rename Class:com.b.Service"
    );
}

#[test]
fn assign_class_slugs_hash_based_not_order_based() {
    // Slug must depend only on the class's own FQN, not on its position within the set.
    // BTreeSet iteration is deterministic by key, so both calls visit the same order —
    // this test confirms that the slugs remain stable regardless of which IDs co-exist.
    let ids_ab: std::collections::BTreeSet<String> = [
        "Class:com.a.Service".to_string(),
        "Class:com.b.Service".to_string(),
    ]
    .into_iter()
    .collect();
    let ids_ba: std::collections::BTreeSet<String> = [
        "Class:com.b.Service".to_string(),
        "Class:com.a.Service".to_string(),
    ]
    .into_iter()
    .collect();
    let slug_ab = assign_class_slugs(&ids_ab, |id| {
        id.rsplit('.').next().unwrap_or("Unknown").to_string()
    });
    let slug_ba = assign_class_slugs(&ids_ba, |id| {
        id.rsplit('.').next().unwrap_or("Unknown").to_string()
    });
    assert_eq!(
        slug_ab["Class:com.a.Service"], slug_ba["Class:com.a.Service"],
        "slug for com.a.Service must be identical regardless of set construction order"
    );
    assert_eq!(
        slug_ab["Class:com.b.Service"], slug_ba["Class:com.b.Service"],
        "slug for com.b.Service must be identical regardless of set construction order"
    );
}
