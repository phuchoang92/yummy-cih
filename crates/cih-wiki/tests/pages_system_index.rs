use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::features::FeatureGroup;
use cih_wiki::graph::WikiGraph;
use cih_wiki::pages::system_index::render_system_index;

fn simple_setup() -> (WikiGraph, Vec<FeatureGroup>) {
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
        name: "order".to_string(),
        qualified_name: None,
        file: String::new(),
        range: Range::default(),
        props: None,
    };
    let g = WikiGraph::build(
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
    );
    let groups = vec![FeatureGroup {
        feature: "order".to_string(),
        community_ids: vec!["Community:0".to_string()],
    }];
    (g, groups)
}

#[test]
fn renders_repo_name_and_feature_table() {
    let (g, groups) = simple_setup();
    let md = render_system_index(&groups, &g, "my-service");
    assert!(md.contains("---\nslug: /\ntitle: my-service"));
    assert!(md.contains("## Features"));
    assert!(md.contains("[Order](order/index.md)"));
}
