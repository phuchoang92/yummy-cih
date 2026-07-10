use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::graph::WikiGraph;
use cih_wiki::pages::feature_ba::render_feature_ba;
use cih_wiki::pages::WikiPageMeta;
use cih_wiki::{CommunityLlmSummary, FeatureLlmSummary};
use std::collections::HashMap;

const TEST_META: WikiPageMeta<'_> = WikiPageMeta {
    enrichment_tier: "graph-only",
    generated_at: "2026-01-01T00:00:00Z",
    graph_version: "test",
};

fn method_node(id: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Method,
        name: "m".to_string(),
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

fn simple_graph() -> (WikiGraph, Vec<String>) {
    let m = method_node("Method:A#do/0");
    let c = comm_node("Community:0", "order");
    let g = WikiGraph::build(
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
    );
    (g, vec!["Community:0".to_string()])
}

#[test]
fn has_correct_frontmatter() {
    let (g, ids) = simple_graph();
    let md = render_feature_ba("order", &ids, &g, None, None, None, None, &TEST_META);
    assert!(md.contains("---\ntitle: Order — Business Analysis"));
}

#[test]
fn includes_process_overview_when_llm_present() {
    let (g, ids) = simple_graph();
    let mut sums = HashMap::new();
    sums.insert(
        "Community:0".to_string(),
        CommunityLlmSummary {
            po: String::new(),
            ba: "Orchestrates the order workflow.".to_string(),
            dev: String::new(),
        },
    );
    let md = render_feature_ba("order", &ids, &g, Some(&sums), None, None, None, &TEST_META);
    assert!(md.contains("## Process Overview"));
    assert!(md.contains("Orchestrates the order workflow"));
}

#[test]
fn renders_feature_level_summary_when_present() {
    let (g, ids) = simple_graph();
    let feature = FeatureLlmSummary {
        po_overview: String::new(),
        po_capabilities: String::new(),
        ba_process_overview: "Feature-wide order process.".to_string(),
        ba_business_rules: "-> Validate order status".to_string(),
    };
    let md = render_feature_ba("order", &ids, &g, None, None, Some(&feature), None, &TEST_META);
    assert!(md.contains("Feature-wide order process"));
    assert!(md.contains("-> Validate order status"));
}
