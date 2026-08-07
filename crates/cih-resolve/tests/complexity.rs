use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_resolve::propagate_loop_depths;

fn method(id: &str, loop_depth: u64) -> Node {
    Node {
        id: NodeId::new(id),
        kind: NodeKind::Method,
        name: id.rsplit('#').next().unwrap_or(id).to_string(),
        qualified_name: None,
        file: "src/main/java/com/acme/A.java".into(),
        range: Range {
            start_line: 1,
            start_col: 0,
            end_line: 2,
            end_col: 1,
        },
        props: (loop_depth > 0).then(|| serde_json::json!({ "loopDepth": loop_depth })),
    }
}

fn calls(src: &str, dst: &str) -> Edge {
    Edge {
        src: NodeId::new(src),
        dst: NodeId::new(dst),
        kind: EdgeKind::Calls,
        confidence: 1.0,
        reason: "test".into(),
        props: None,
    }
}

fn tld(nodes: &[Node], id: &str) -> u64 {
    nodes
        .iter()
        .find(|n| n.id.as_str() == id)
        .and_then(|n| n.props.as_ref())
        .and_then(|p| p.get("transitiveLoopDepth"))
        .and_then(|v| v.as_u64())
        .expect("transitiveLoopDepth present")
}

fn is_recursive(nodes: &[Node], id: &str) -> bool {
    nodes
        .iter()
        .find(|n| n.id.as_str() == id)
        .and_then(|n| n.props.as_ref())
        .and_then(|p| p.get("isRecursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[test]
fn tld_is_additive_along_the_deepest_callee_chain() {
    // a(ld=1) -> b(ld=2) -> c(ld=0); a also -> d(ld=0). tld(a) = 1 + max(2+0, 0).
    let mut nodes = vec![
        method("Method:A#a/0", 1),
        method("Method:A#b/0", 2),
        method("Method:A#c/0", 0),
        method("Method:A#d/0", 0),
    ];
    let edges = vec![
        calls("Method:A#a/0", "Method:A#b/0"),
        calls("Method:A#a/0", "Method:A#d/0"),
        calls("Method:A#b/0", "Method:A#c/0"),
    ];

    propagate_loop_depths(&mut nodes, &edges);

    assert_eq!(tld(&nodes, "Method:A#a/0"), 3);
    assert_eq!(tld(&nodes, "Method:A#b/0"), 2);
    assert_eq!(tld(&nodes, "Method:A#c/0"), 0);
    assert!(!is_recursive(&nodes, "Method:A#a/0"));
}

#[test]
fn cycles_mark_the_revisited_node_recursive_and_contribute_zero() {
    // a -> b -> a; the back-edge target is flagged, values stay finite.
    let mut nodes = vec![method("Method:A#a/0", 1), method("Method:A#b/0", 1)];
    let edges = vec![
        calls("Method:A#a/0", "Method:A#b/0"),
        calls("Method:A#b/0", "Method:A#a/0"),
    ];

    propagate_loop_depths(&mut nodes, &edges);

    assert_eq!(tld(&nodes, "Method:A#a/0"), 2);
    assert_eq!(tld(&nodes, "Method:A#b/0"), 1);
    assert!(is_recursive(&nodes, "Method:A#a/0"));
}

#[test]
fn tld_caps_at_twenty() {
    // 30 chained methods each with loopDepth 1 — the sum saturates at 20.
    let ids: Vec<String> = (0..30).map(|i| format!("Method:A#m{i}/0")).collect();
    let mut nodes: Vec<Node> = ids.iter().map(|id| method(id, 1)).collect();
    let edges: Vec<Edge> = ids.windows(2).map(|w| calls(&w[0], &w[1])).collect();

    propagate_loop_depths(&mut nodes, &edges);

    assert_eq!(tld(&nodes, &ids[0]), 20);
}

#[test]
fn deep_call_chains_do_not_overflow_the_stack() {
    // A 200k-deep CALLS chain: the traversal must be iterative — the previous
    // recursive DFS overflowed the thread stack at repository scale.
    const DEPTH: usize = 200_000;
    let ids: Vec<String> = (0..DEPTH).map(|i| format!("Method:A#m{i}/0")).collect();
    let mut nodes: Vec<Node> = ids.iter().map(|id| method(id, 0)).collect();
    let edges: Vec<Edge> = ids.windows(2).map(|w| calls(&w[0], &w[1])).collect();

    propagate_loop_depths(&mut nodes, &edges);

    assert_eq!(tld(&nodes, &ids[0]), 0);
    assert_eq!(tld(&nodes, &ids[DEPTH - 1]), 0);
}
