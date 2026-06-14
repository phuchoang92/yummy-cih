use std::collections::HashMap;

use cih_core::{Node, NodeId, NodeKind};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

pub fn score_entry_points(
    nodes: &[Node],
    digraph: &DiGraph<NodeId, f32>,
    node_index: &HashMap<NodeId, NodeIndex>,
) -> Vec<(NodeId, f64)> {
    let by_id: HashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();
    let mut scored = Vec::new();

    for node in nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Method | NodeKind::Constructor))
    {
        let Some(&idx) = node_index.get(&node.id) else {
            continue;
        };
        let callees = digraph.neighbors_directed(idx, Direction::Outgoing).count() as f64;
        if callees == 0.0 {
            continue;
        }
        let callers = digraph.neighbors_directed(idx, Direction::Incoming).count() as f64;
        let name = by_id
            .get(&node.id)
            .map(|n| n.name.as_str())
            .unwrap_or(node.id.as_str());
        let score = (callees / (callers + 1.0)) * name_multiplier(name);
        scored.push((node.id.clone(), score));
    }

    scored.sort_by(|(a_id, a_score), (b_id, b_score)| {
        b_score
            .partial_cmp(a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a_id.as_str().cmp(b_id.as_str()))
    });
    scored.truncate(200);
    scored
}

fn name_multiplier(name: &str) -> f64 {
    if is_entry_name(name) {
        1.5
    } else if is_utility_name(name) {
        0.3
    } else {
        1.0
    }
}

fn is_entry_name(name: &str) -> bool {
    if name == "main" {
        return true;
    }
    const STARTS: &[&str] = &[
        "main", "init", "execute", "run", "start", "handle", "process", "perform", "dispatch",
        "trigger", "fire", "emit",
    ];
    const ENDS: &[&str] = &["Handler", "Controller", "Listener", "Endpoint"];
    STARTS.iter().any(|p| starts_entry(name, p)) || ENDS.iter().any(|s| name.ends_with(s))
}

fn starts_entry(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty()
        || rest
            .chars()
            .next()
            .map(|c| c == '_' || c.is_ascii_uppercase())
            .unwrap_or(false)
}

fn is_utility_name(name: &str) -> bool {
    const STARTS: &[&str] = &[
        "get", "set", "is", "has", "to", "from", "format", "parse", "validate", "convert", "log",
        "debug",
    ];
    const ENDS: &[&str] = &["Helper", "Util", "Utils"];
    STARTS.iter().any(|p| name.starts_with(p)) || ENDS.iter().any(|s| name.ends_with(s))
}
