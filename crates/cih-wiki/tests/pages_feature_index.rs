use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::graph::WikiGraph;
use cih_wiki::pages::feature_index::render_feature_index;
use std::collections::HashMap;

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
        &[m.clone()],
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

#[test]
fn renders_with_correct_frontmatter() {
    let g = simple_graph();
    let ids = vec!["Community:0".to_string()];
    let mut dev_paths = HashMap::new();
    dev_paths.insert(
        "Community:0".to_string(),
        "order/dev/order-service".to_string(),
    );
    let md = render_feature_index("order", &ids, &dev_paths, &g);
    assert!(md.contains("---\ntitle: Order — Feature Overview"));
    assert!(md.contains("Order — Feature Overview"));
    assert!(md.contains("order-service"));
    assert!(md.contains("order/dev/order-service.md"));
}
