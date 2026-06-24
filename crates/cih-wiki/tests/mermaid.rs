use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::graph::WikiGraph;
use cih_wiki::mermaid::{community_call_diagram, process_flow_diagram, sanitize};

fn simple_node(id: &str, kind: NodeKind, name: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind,
        name: name.to_string(),
        qualified_name: None,
        file: String::new(),
        range: Range::default(),
        props: None,
    }
}

#[test]
fn sanitize_escapes_quotes_and_dashes() {
    assert!(!sanitize("Hello \"world\"").contains('"'));
    assert!(!sanitize("a--b").contains("--"));
    assert!(!sanitize("<type>").contains('<'));
}

#[test]
fn community_call_diagram_requires_at_least_two_communities() {
    let g = WikiGraph::build(&[], &[], &[], &[]);
    assert!(community_call_diagram(&g, "Community:0").is_none());
}

#[test]
fn community_call_diagram_produces_flowchart() {
    let c0 = simple_node("Community:0", NodeKind::Community, "order");
    let c1 = simple_node("Community:1", NodeKind::Community, "payment");
    let m0 = simple_node("Method:a#f/0", NodeKind::Method, "f");
    let m1 = simple_node("Method:b#g/0", NodeKind::Method, "g");
    let edges = [
        Edge {
            src: m0.id.clone(),
            dst: c0.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: m1.id.clone(),
            dst: c1.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: m0.id.clone(),
            dst: m1.id.clone(),
            kind: EdgeKind::Calls,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
    ];
    let g = WikiGraph::build(&[m0, m1], &edges[2..], &[c0, c1], &edges[..2]);
    let result = community_call_diagram(&g, "Community:0");
    assert!(result.is_some());
    let diagram = result.unwrap();
    assert!(diagram.starts_with("flowchart LR"));
    assert!(diagram.contains("order") || diagram.contains("Community_0"));
}

#[test]
fn process_flow_diagram_returns_none_for_empty_graph() {
    let g = WikiGraph::build(&[], &[], &[], &[]);
    assert!(process_flow_diagram(&g, &[], false).is_none());
}
