use super::*;
use cih_core::{Node, NodeId, Range};
use cih_graph_store::{
    GraphOverviewEdge, GraphOverviewNode, GraphProjection, GraphProjectionEdge,
    GraphProjectionNode, ProjectionNodeRole, ProjectionScope,
};

fn node(id: &str, kind: NodeKind, file: &str, degree: u64) -> GraphOverviewNode {
    GraphOverviewNode {
        node: Node {
            id: NodeId::new(id),
            kind,
            name: id.to_string(),
            qualified_name: None,
            file: file.to_string(),
            range: Range::default(),
            props: None,
        },
        degree,
    }
}

#[test]
fn layout_is_deterministic_and_finite() {
    let overview = GraphOverview {
        nodes: vec![
            node("Route:GET /orders", NodeKind::Route, "", 1),
            node("Method:Orders#list/0", NodeKind::Method, "src/orders.rs", 2),
            node("Method:Repo#all/0", NodeKind::Method, "src/repo.rs", 1),
        ],
        edges: vec![
            GraphOverviewEdge {
                source: NodeId::new("Route:GET /orders"),
                target: NodeId::new("Method:Orders#list/0"),
                kind: EdgeKind::HandlesRoute,
            },
            GraphOverviewEdge {
                source: NodeId::new("Method:Orders#list/0"),
                target: NodeId::new("Method:Repo#all/0"),
                kind: EdgeKind::Calls,
            },
        ],
        total_nodes: 3,
        total_edges: 2,
        truncated: false,
    };
    let first = compute(overview.clone());
    let second = compute(overview);
    assert_eq!(first.nodes.len(), 3);
    assert_eq!(first.edges.len(), 2);
    for (a, b) in first.nodes.iter().zip(&second.nodes) {
        assert!(a.x.is_finite() && a.y.is_finite() && a.z.is_finite());
        assert_eq!((a.x, a.y, a.z), (b.x, b.y, b.z));
    }
    assert!(first
        .edges
        .iter()
        .all(|edge| edge.source < 3 && edge.target < 3));
}

#[test]
fn empty_layout_keeps_totals() {
    let layout = compute(GraphOverview {
        nodes: Vec::new(),
        edges: Vec::new(),
        total_nodes: 42,
        total_edges: 99,
        truncated: true,
    });
    assert!(layout.nodes.is_empty());
    assert_eq!(layout.total_nodes, 42);
    assert!(layout.truncated);
}

#[test]
fn projection_seed_is_deterministic_and_endpoint_closed() {
    let graph = GraphProjection {
        nodes: vec![
            GraphProjectionNode {
                id: NodeId::new("Community:a"),
                kind: NodeKind::Community,
                name: "A".into(),
                role: ProjectionNodeRole::Aggregate,
                member_count: 400_000,
                degree: 12,
                expandable: true,
            },
            GraphProjectionNode {
                id: NodeId::new("Community:b"),
                kind: NodeKind::Community,
                name: "B".into(),
                role: ProjectionNodeRole::Boundary,
                member_count: 20,
                degree: 12,
                expandable: true,
            },
        ],
        edges: vec![GraphProjectionEdge {
            source: NodeId::new("Community:a"),
            target: NodeId::new("Community:b"),
            kind: EdgeKind::Calls,
            count: 12,
        }],
        total_nodes: 2,
        total_edges: 1,
        truncated: false,
    };
    let first = compute_projection(
        graph.clone(),
        ProjectionScope::Repository,
        None,
        Some("epoch-1".into()),
    );
    let second = compute_projection(
        graph,
        ProjectionScope::Repository,
        None,
        Some("epoch-1".into()),
    );
    assert_eq!(first.nodes.len(), 2);
    assert_eq!(first.edges.len(), 1);
    assert_eq!(first.edges[0].source, 0);
    assert_eq!(first.edges[0].target, 1);
    assert_eq!(
        (first.nodes[0].x, first.nodes[0].y),
        (second.nodes[0].x, second.nodes[0].y)
    );
}

#[test]
fn disconnected_nodes_produce_valid_layout() {
    let overview = GraphOverview {
        nodes: vec![
            node("Class:Alpha", NodeKind::Class, "src/alpha.rs", 0),
            node("Class:Beta", NodeKind::Class, "src/beta.rs", 0),
            node("Class:Gamma", NodeKind::Class, "lib/gamma.rs", 0),
        ],
        edges: Vec::new(),
        total_nodes: 3,
        total_edges: 0,
        truncated: false,
    };
    let layout = compute(overview);
    assert_eq!(layout.nodes.len(), 3);
    assert!(layout.edges.is_empty());
    for n in &layout.nodes {
        assert!(n.x.is_finite() && n.y.is_finite() && n.z.is_finite());
        assert!(n.size > 0.0);
        assert!(!n.color.is_empty());
    }
}

#[test]
fn duplicate_edges_are_deduplicated() {
    let a = NodeId::new("Method:A#run/0");
    let b = NodeId::new("Method:B#save/0");
    let overview = GraphOverview {
        nodes: vec![
            node("Method:A#run/0", NodeKind::Method, "src/a.rs", 2),
            node("Method:B#save/0", NodeKind::Method, "src/b.rs", 2),
        ],
        edges: vec![
            GraphOverviewEdge {
                source: a.clone(),
                target: b.clone(),
                kind: EdgeKind::Calls,
            },
            GraphOverviewEdge {
                source: a.clone(),
                target: b.clone(),
                kind: EdgeKind::Calls,
            },
            GraphOverviewEdge {
                source: a,
                target: b,
                kind: EdgeKind::Imports,
            },
        ],
        total_nodes: 2,
        total_edges: 3,
        truncated: false,
    };
    let layout = compute(overview);
    assert_eq!(layout.edges.len(), 2);
    assert!(layout.edges.iter().all(|e| e.source < 2 && e.target < 2));
}

#[test]
fn unknown_kind_gets_default_size_and_color() {
    let overview = GraphOverview {
        nodes: vec![node("Other:widget", NodeKind::Other, "ext/widget.py", 5)],
        edges: Vec::new(),
        total_nodes: 1,
        total_edges: 0,
        truncated: false,
    };
    let layout = compute(overview);
    assert_eq!(layout.nodes.len(), 1);
    let n = &layout.nodes[0];
    assert_eq!(n.kind, "Other");
    assert!(
        n.size > 3.0 && n.size < 20.0,
        "size {} out of expected range",
        n.size
    );
    assert_eq!(n.color, "#ffc070");
}

#[test]
fn large_degree_does_not_overflow() {
    let overview = GraphOverview {
        nodes: vec![node(
            "Method:Hub#dispatch/0",
            NodeKind::Method,
            "src/hub.rs",
            999_999,
        )],
        edges: Vec::new(),
        total_nodes: 1,
        total_edges: 0,
        truncated: false,
    };
    let layout = compute(overview);
    assert_eq!(layout.nodes.len(), 1);
    let n = &layout.nodes[0];
    assert!(n.x.is_finite() && n.y.is_finite() && n.z.is_finite());
    assert!(n.size.is_finite() && n.size > 0.0);
    assert_eq!(n.color, "#80a0ff");
}
