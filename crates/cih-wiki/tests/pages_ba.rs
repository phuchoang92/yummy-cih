use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::graph::WikiGraph;
use cih_wiki::pages::ba::{render_ba_community, render_ba_community_json};
use cih_wiki::CommunityLlmSummary;
use std::collections::BTreeMap;

fn make_node(id: &str, kind: NodeKind, name: &str) -> Node {
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

fn member_edge(sym_id: &str, comm_id: &str) -> Edge {
    Edge {
        src: NodeId::new(sym_id.to_string()),
        dst: NodeId::new(comm_id.to_string()),
        kind: EdgeKind::MemberOf,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }
}

fn two_community_graph() -> WikiGraph {
    let sym_a = make_node("Method:A#doA/0", NodeKind::Method, "doA");
    let sym_b = make_node("Method:B#doB/0", NodeKind::Method, "doB");
    let comm_a = make_node("Community:0", NodeKind::Community, "svc-a");
    let comm_b = make_node("Community:1", NodeKind::Community, "svc-b");
    let nodes = [sym_a.clone(), sym_b.clone()];
    let edges = [Edge {
        src: NodeId::new("Method:A#doA/0".to_string()),
        dst: NodeId::new("Method:B#doB/0".to_string()),
        kind: EdgeKind::Calls,
        confidence: 1.0,
        reason: String::new(),
        props: None,
    }];
    let comm_nodes = [comm_a, comm_b];
    let comm_edges = [
        member_edge("Method:A#doA/0", "Community:0"),
        member_edge("Method:B#doB/0", "Community:1"),
    ];
    WikiGraph::build(&nodes, &edges, &comm_nodes, &comm_edges)
}

fn slug_map() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("Community:0".to_string(), "svc-a".to_string());
    m.insert("Community:1".to_string(), "svc-b".to_string());
    m
}

#[test]
fn render_ba_community_shows_inter_community_calls() {
    let g = two_community_graph();
    let comm_a = g
        .community_nodes
        .iter()
        .find(|n| n.name == "svc-a")
        .unwrap()
        .clone();
    let md = render_ba_community(&g, &comm_a, &slug_map(), None);
    assert!(md.contains("Consumes"), "has consumes section");
    assert!(md.contains("svc-b"), "mentions callee community");
}

#[test]
fn render_ba_community_writes_sidecar_shape() {
    let g = two_community_graph();
    let comm_a = g
        .community_nodes
        .iter()
        .find(|n| n.name == "svc-a")
        .unwrap()
        .clone();
    let val = render_ba_community_json(&g, &comm_a);
    assert_eq!(val["format"], "community-slice");
    assert!(val["nodes"].is_array());
    assert!(val["links"].is_array());
}

#[test]
fn render_ba_community_shows_data_access_when_present() {
    let sym_a = make_node("Method:A#doA/0", NodeKind::Method, "doA");
    let dbq = make_node("DbQuery:A#SQL", NodeKind::DbQuery, "SQL");
    let tbl = make_node("DbTable:ACCOUNTS", NodeKind::DbTable, "ACCOUNTS");
    let comm_a = make_node("Community:0", NodeKind::Community, "svc-a");
    let nodes = [sym_a.clone(), dbq.clone(), tbl.clone()];
    let edges = [
        Edge {
            src: sym_a.id.clone(),
            dst: dbq.id.clone(),
            kind: EdgeKind::ExecutesQuery,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
        Edge {
            src: dbq.id.clone(),
            dst: tbl.id.clone(),
            kind: EdgeKind::WritesTable,
            confidence: 1.0,
            reason: String::new(),
            props: None,
        },
    ];
    let comm_edges = [member_edge("Method:A#doA/0", "Community:0")];
    let g = WikiGraph::build(&nodes, &edges, &[comm_a], &comm_edges);
    let comm = g.community_nodes[0].clone();
    let mut sm = BTreeMap::new();
    sm.insert("Community:0".to_string(), "svc-a".to_string());
    let md = render_ba_community(&g, &comm, &sm, None);
    assert!(md.contains("## Data Access"), "has data access section");
    assert!(md.contains("ACCOUNTS"), "has table name");
    assert!(md.contains("✓"), "has check mark for write");
}

#[test]
fn render_ba_community_omits_data_access_when_none() {
    let g = two_community_graph();
    let comm_a = g
        .community_nodes
        .iter()
        .find(|n| n.name == "svc-a")
        .unwrap()
        .clone();
    let md = render_ba_community(&g, &comm_a, &slug_map(), None);
    assert!(
        !md.contains("## Data Access"),
        "no data access when no db tables"
    );
}

#[test]
fn render_ba_community_inserts_workflow_summary_when_present() {
    let g = two_community_graph();
    let comm_a = g
        .community_nodes
        .iter()
        .find(|n| n.name == "svc-a")
        .unwrap()
        .clone();
    let llm = CommunityLlmSummary {
        po: String::new(),
        ba: "Orchestrates the order workflow.".to_string(),
        dev: String::new(),
    };
    let md = render_ba_community(&g, &comm_a, &slug_map(), Some(&llm));
    assert!(
        md.contains("## Workflow Summary"),
        "has workflow summary section"
    );
    assert!(
        md.contains("Orchestrates the order workflow"),
        "has llm text"
    );
}
