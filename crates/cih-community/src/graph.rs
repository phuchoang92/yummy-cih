use std::collections::{HashMap, HashSet};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};
use petgraph::graph::{DiGraph, NodeIndex, UnGraph};

const LARGE_GRAPH_THRESHOLD: usize = 10_000;

pub fn build_community_graph(
    nodes: &[Node],
    edges: &[Edge],
    large: bool,
    min_confidence: f32,
) -> (UnGraph<NodeId, f32>, HashMap<NodeId, NodeIndex>) {
    let eligible: HashSet<NodeId> = nodes
        .iter()
        .filter(|n| is_community_symbol(n.kind))
        .map(|n| n.id.clone())
        .collect();

    let mut degree: HashMap<NodeId, usize> = HashMap::new();
    if large {
        for edge in community_edges(edges, min_confidence, true) {
            if edge.src != edge.dst && eligible.contains(&edge.src) && eligible.contains(&edge.dst)
            {
                *degree.entry(edge.src.clone()).or_default() += 1;
                *degree.entry(edge.dst.clone()).or_default() += 1;
            }
        }
    }

    let mut graph = UnGraph::<NodeId, f32>::new_undirected();
    let mut index = HashMap::new();
    for node in nodes.iter().filter(|n| is_community_symbol(n.kind)) {
        if large && degree.get(&node.id).copied().unwrap_or(0) <= 1 {
            continue;
        }
        let idx = graph.add_node(node.id.clone());
        index.insert(node.id.clone(), idx);
    }

    for edge in community_edges(edges, min_confidence, large) {
        if edge.src == edge.dst {
            continue;
        }
        let (Some(&src), Some(&dst)) = (index.get(&edge.src), index.get(&edge.dst)) else {
            continue;
        };
        if let Some(edge_idx) = graph.find_edge(src, dst) {
            if let Some(weight) = graph.edge_weight_mut(edge_idx) {
                *weight += edge.confidence.max(0.01);
            }
        } else {
            graph.add_edge(src, dst, edge.confidence.max(0.01));
        }
    }

    (graph, index)
}

pub fn build_calls_digraph(
    nodes: &[Node],
    edges: &[Edge],
    min_confidence: f32,
) -> (DiGraph<NodeId, f32>, HashMap<NodeId, NodeIndex>) {
    let mut graph = DiGraph::<NodeId, f32>::new();
    let mut index = HashMap::new();
    for node in nodes.iter().filter(|n| is_callable(n.kind)) {
        let idx = graph.add_node(node.id.clone());
        index.insert(node.id.clone(), idx);
    }

    for edge in edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls && e.confidence >= min_confidence)
    {
        if edge.src == edge.dst {
            continue;
        }
        let (Some(&src), Some(&dst)) = (index.get(&edge.src), index.get(&edge.dst)) else {
            continue;
        };
        graph.add_edge(src, dst, edge.confidence.max(0.01));
    }

    (graph, index)
}

pub fn is_large_graph(nodes: &[Node]) -> bool {
    symbol_node_count(nodes) > LARGE_GRAPH_THRESHOLD
}

pub fn symbol_node_count(nodes: &[Node]) -> usize {
    nodes.iter().filter(|n| is_community_symbol(n.kind)).count()
}

pub fn is_community_symbol(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class | NodeKind::Interface | NodeKind::Method | NodeKind::Constructor
    )
}

fn is_callable(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Constructor)
}

fn community_edges(
    edges: &[Edge],
    min_confidence: f32,
    large: bool,
) -> impl Iterator<Item = &Edge> {
    edges.iter().filter(move |e| {
        matches!(
            e.kind,
            EdgeKind::Calls | EdgeKind::Extends | EdgeKind::Implements
        ) && (!large || e.confidence >= min_confidence)
    })
}
