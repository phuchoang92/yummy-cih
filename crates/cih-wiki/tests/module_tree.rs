use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind, Range};
use cih_wiki::graph::WikiGraph;
use cih_wiki::module_tree::*;

fn method_node(id: &str, file: &str) -> Node {
    Node {
        id: NodeId::new(id.to_string()),
        kind: NodeKind::Method,
        name: "handle".to_string(),
        qualified_name: None,
        file: file.to_string(),
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

#[test]
fn graph_module_tree_uses_feature_groups() {
    let m = method_node(
        "Method:com.example.modules.order.OrderService#save/0",
        "src/main/java/com/example/modules/order/OrderService.java",
    );
    let comm = comm_node("Community:0", "Order");
    let graph = WikiGraph::build(
        std::slice::from_ref(&m),
        &[],
        &[comm],
        &[member_edge(m.id.as_str(), "Community:0")],
    );
    let tree = build_graph_module_tree(&graph, None, "g1", "c1", Some("abc".into()));
    assert_eq!(tree.modules.len(), 1);
    assert_eq!(tree.modules[0].slug, "order");
    assert_eq!(tree.modules[0].community_ids, vec!["Community:0"]);
    validate_module_tree(&tree, &graph).unwrap();
}

#[test]
fn validation_rejects_unknown_community() {
    let graph = WikiGraph::build(&[], &[], &[], &[]);
    let tree = WikiModuleTree {
        schema_version: 1,
        generated_at: "now".to_string(),
        source: ModuleTreeSource::Graph,
        repo_commit: None,
        graph_version: "g".to_string(),
        community_version: "c".to_string(),
        modules: vec![WikiModuleNode {
            id: "feature:bad".to_string(),
            slug: "bad".to_string(),
            title: "Bad".to_string(),
            description: None,
            community_ids: vec!["Community:999".to_string()],
            file_paths: vec![],
            children: vec![],
        }],
    };
    assert!(validate_module_tree(&tree, &graph).is_err());
}

#[test]
fn validation_rejects_unsafe_paths() {
    let comm = comm_node("Community:0", "Order");
    let graph = WikiGraph::build(&[], &[], &[comm], &[]);
    let tree = WikiModuleTree {
        schema_version: 1,
        generated_at: "now".to_string(),
        source: ModuleTreeSource::Graph,
        repo_commit: None,
        graph_version: "g".to_string(),
        community_version: "c".to_string(),
        modules: vec![WikiModuleNode {
            id: "feature:bad".to_string(),
            slug: "bad".to_string(),
            title: "Bad".to_string(),
            description: None,
            community_ids: vec!["Community:0".to_string()],
            file_paths: vec!["../Secret.java".to_string()],
            children: vec![],
        }],
    };
    assert!(validate_module_tree(&tree, &graph).is_err());
}
