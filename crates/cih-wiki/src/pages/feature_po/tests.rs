use super::*;
use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};

fn method_node(id: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Method,
        name: id
            .split('#')
            .nth(1)
            .unwrap_or("m")
            .split('/')
            .next()
            .unwrap_or("m")
            .to_string(),
        qualified_name: None,
        file: String::new(),
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

fn simple_graph() -> (WikiGraph, Vec<String>) {
    let m = method_node("Method:A#do/0");
    let c = comm_node("Community:0", "payment");
    let g = WikiGraph::build(
        &[m.clone()],
        &[],
        &[c],
        &[member_edge(m.id.as_str(), "Community:0")],
    );
    (g, vec!["Community:0".to_string()])
}

#[test]
fn renders_overview_when_llm_present() {
    let (g, ids) = simple_graph();
    let mut sums = HashMap::new();
    sums.insert(
        "Community:0".to_string(),
        CommunityLlmSummary {
            po: "Handles payment flows.".to_string(),
            ba: String::new(),
            dev: String::new(),
        },
    );
    let md = render_feature_po("payment", &ids, &g, Some(&sums), None, None, None);
    assert!(md.contains("## Overview"));
    assert!(md.contains("Handles payment flows"));
}

#[test]
fn renders_feature_level_summary_when_present() {
    let (g, ids) = simple_graph();
    let feature = FeatureLlmSummary {
        po_overview: "Feature-wide payment overview.".to_string(),
        po_capabilities: "-> Submit payment".to_string(),
        ba_process_overview: String::new(),
        ba_business_rules: String::new(),
    };
    let md = render_feature_po("payment", &ids, &g, None, None, Some(&feature), None);
    assert!(md.contains("Feature-wide payment overview"));
    assert!(md.contains("-> Submit payment"));
}

#[test]
fn omits_overview_when_no_llm() {
    let (g, ids) = simple_graph();
    let md = render_feature_po("payment", &ids, &g, None, None, None, None);
    assert!(!md.contains("## Overview"));
}

#[test]
fn has_correct_frontmatter() {
    let (g, ids) = simple_graph();
    let md = render_feature_po("payment", &ids, &g, None, None, None, None);
    assert!(md.contains("---\ntitle: Payment — Business Overview"));
}
