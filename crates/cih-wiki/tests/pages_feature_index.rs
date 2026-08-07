use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::graph::WikiGraph;
use cih_wiki::pages::feature_index::render_feature_index;
use cih_wiki::pages::WikiPageMeta;

fn simple_graph() -> WikiGraph {
    let m = Node {
        id: NodeId::new("Method:A#do/0".to_string()),
        kind: NodeKind::Method,
        name: "do".to_string(),
        qualified_name: None,
        file: String::new(),
        range: Range::default(),
        props: None,
    };
    let c = Node {
        id: NodeId::new("Community:0".to_string()),
        kind: NodeKind::Community,
        name: "order-service".to_string(),
        qualified_name: None,
        file: String::new(),
        range: Range::default(),
        props: None,
    };
    WikiGraph::build(
        std::slice::from_ref(&m),
        &[],
        &[c],
        &[Edge {
            src: m.id.clone(),
            dst: NodeId::new("Community:0".to_string()),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        }],
    )
}

fn graph_only_meta() -> WikiPageMeta<'static> {
    WikiPageMeta {
        enrichment_tier: "graph-only",
        graph_version: "test-v1",
    }
}

#[test]
fn renders_with_correct_frontmatter() {
    let g = simple_graph();
    let ids = vec!["Community:0".to_string()];
    // class_dev_links: (class_name, dev_slug) where dev_slug is relative to the feature dir.
    let class_dev_links = vec![("OrderService".to_string(), "dev/order-service".to_string())];
    let meta = graph_only_meta();
    let md = render_feature_index("order", &ids, &class_dev_links, &g, &meta);
    assert!(md.contains("---\ntitle: Order — Feature Overview"));
    assert!(md.contains("Order — Feature Overview"));
    assert!(md.contains("OrderService"));
    // Link must use the dev/-relative path, NOT the full feature/dev/... path.
    assert!(
        md.contains("dev/order-service.md"),
        "link must use relative dev/... path, got:\n{md}"
    );
    assert!(
        !md.contains("order/dev/order-service.md"),
        "link must NOT include the feature prefix (would resolve to wrong path)"
    );
    // Provenance fields must appear in front matter.
    assert!(
        md.contains("cih_enrichment: graph-only"),
        "must contain enrichment tier"
    );
    assert!(
        md.contains("cih_graph_version: test-v1"),
        "must contain graph version"
    );
}

#[test]
fn renders_empty_class_list_without_classes_section() {
    let g = simple_graph();
    let ids = vec!["Community:0".to_string()];
    let meta = graph_only_meta();
    let md = render_feature_index("order", &ids, &[], &g, &meta);
    assert!(
        !md.contains("## Classes"),
        "empty class list must omit the Classes section"
    );
    assert!(
        md.contains("Business Overview"),
        "role pages still rendered"
    );
}
