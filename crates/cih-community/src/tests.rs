use super::*;
use cih_core::{method_id, type_id};

fn class_node(fqcn: &str, file: &str) -> Node {
    Node {
        id: type_id(NodeKind::Class, fqcn),
        kind: NodeKind::Class,
        name: fqcn.rsplit('.').next().unwrap_or(fqcn).to_string(),
        qualified_name: Some(fqcn.to_string()),
        file: file.to_string(),
        range: Range::default(),
        props: None,
    }
}

fn method_node(fqcn: &str, name: &str) -> Node {
    Node {
        id: method_id(fqcn, name, 0),
        kind: NodeKind::Method,
        name: name.to_string(),
        qualified_name: Some(format!("{fqcn}#{name}/0")),
        file: format!("src/main/java/{}.java", fqcn.replace('.', "/")),
        range: Range::default(),
        props: None,
    }
}

fn edge(src: &NodeId, dst: &NodeId, kind: EdgeKind, confidence: f32) -> Edge {
    Edge {
        src: src.clone(),
        dst: dst.clone(),
        kind,
        confidence,
        reason: String::new(),
            props: None,
    }
}

fn call(src: &Node, dst: &Node) -> Edge {
    edge(&src.id, &dst.id, EdgeKind::Calls, 1.0)
}

#[test]
fn community_detection_splits_two_cliques() {
    let nodes = vec![
        class_node("com.acme.a.A1", "src/main/java/com/acme/a/A1.java"),
        class_node("com.acme.a.A2", "src/main/java/com/acme/a/A2.java"),
        class_node("com.acme.a.A3", "src/main/java/com/acme/a/A3.java"),
        class_node("com.acme.b.B1", "src/main/java/com/acme/b/B1.java"),
        class_node("com.acme.b.B2", "src/main/java/com/acme/b/B2.java"),
        class_node("com.acme.b.B3", "src/main/java/com/acme/b/B3.java"),
    ];
    let mut edges = Vec::new();
    for (a, b) in [(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5)] {
        edges.push(edge(&nodes[a].id, &nodes[b].id, EdgeKind::Calls, 1.0));
    }
    edges.push(edge(&nodes[2].id, &nodes[3].id, EdgeKind::Calls, 0.05));

    let out = detect_communities(
        &nodes,
        &edges,
        &CommunityConfig {
            max_iterations: 20,
            ..CommunityConfig::default()
        },
    );
    assert_eq!(out.nodes.len(), 2);
    assert_eq!(out.edges.len(), 6);
}

#[test]
fn seeded_rng_is_deterministic() {
    let nodes = vec![
        class_node("com.acme.A", "src/main/java/com/acme/A.java"),
        class_node("com.acme.B", "src/main/java/com/acme/B.java"),
        class_node("com.acme.C", "src/main/java/com/acme/C.java"),
    ];
    let edges = vec![call(&nodes[0], &nodes[1]), call(&nodes[1], &nodes[2])];
    let first = detect_communities(&nodes, &edges, &CommunityConfig::default());
    let second = detect_communities(&nodes, &edges, &CommunityConfig::default());
    let first_edges: Vec<_> = first
        .edges
        .iter()
        .map(|e| (e.src.as_str().to_string(), e.dst.as_str().to_string()))
        .collect();
    let second_edges: Vec<_> = second
        .edges
        .iter()
        .map(|e| (e.src.as_str().to_string(), e.dst.as_str().to_string()))
        .collect();
    assert_eq!(first_edges, second_edges);
}

#[test]
fn singleton_communities_are_discarded() {
    let nodes = vec![class_node(
        "com.acme.Alone",
        "src/main/java/com/acme/Alone.java",
    )];
    let out = detect_communities(&nodes, &[], &CommunityConfig::default());
    assert!(out.nodes.is_empty());
    assert!(out.edges.is_empty());
}

#[test]
fn process_trace_min_steps_enforced() {
    let a = method_node("com.acme.A", "handle");
    let b = method_node("com.acme.B", "work");
    let c = method_node("com.acme.C", "done");

    let short = trace_processes(
        &[a.clone(), b.clone()],
        &[call(&a, &b)],
        &[],
        &ProcessConfig::for_symbol_count(2),
        &EntrypointRegistry::default(),
    );
    assert!(short.nodes.is_empty());

    let long = trace_processes(
        &[a.clone(), b.clone(), c.clone()],
        &[call(&a, &b), call(&b, &c)],
        &[],
        &ProcessConfig::for_symbol_count(3),
        &EntrypointRegistry::default(),
    );
    assert_eq!(long.nodes.len(), 1);
    assert_eq!(long.edges.len(), 3);
}

#[test]
fn process_cycle_prevention() {
    let a = method_node("com.acme.A", "handle");
    let b = method_node("com.acme.B", "work");
    let c = method_node("com.acme.C", "done");
    let out = trace_processes(
        &[a.clone(), b.clone(), c.clone()],
        &[call(&a, &b), call(&b, &c), call(&c, &a)],
        &[],
        &ProcessConfig::for_symbol_count(3),
        &EntrypointRegistry::default(),
    );
    assert!(!out.nodes.is_empty());
    assert!(out.nodes.len() < 10);
}

#[test]
fn process_cross_community() {
    let a = method_node("com.acme.A", "handle");
    let b = method_node("com.acme.B", "work");
    let c = method_node("com.acme.C", "done");
    let memberships = vec![
        (a.id.clone(), community_id(0)),
        (b.id.clone(), community_id(0)),
        (c.id.clone(), community_id(1)),
    ];
    let out = trace_processes(
        &[a.clone(), b.clone(), c.clone()],
        &[call(&a, &b), call(&b, &c)],
        &memberships,
        &ProcessConfig::for_symbol_count(3),
        &EntrypointRegistry::default(),
    );
    let process_type = out.nodes[0]
        .props
        .as_ref()
        .and_then(|p| p.get("process_type"))
        .and_then(|v| v.as_str());
    assert_eq!(process_type, Some("cross_community"));
}

#[test]
fn process_dedup_keeps_longest() {
    let a = NodeId::new("Method:A#a/0");
    let b = NodeId::new("Method:B#b/0");
    let c = NodeId::new("Method:C#c/0");
    let d = NodeId::new("Method:D#d/0");
    let mut graph = petgraph::graph::DiGraph::<NodeId, f32>::new();
    let ai = graph.add_node(a);
    let bi = graph.add_node(b);
    let ci = graph.add_node(c);
    let di = graph.add_node(d);
    let deduped =
        crate::bfs::deduplicate_traces(vec![vec![ai, bi, ci], vec![ai, bi, ci, di]], &graph);
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0], vec![ai, bi, ci, di]);
}
