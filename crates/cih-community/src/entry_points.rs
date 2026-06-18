use std::collections::{BTreeSet, HashMap};

use cih_core::{Edge, EdgeKind, Node, NodeId, NodeKind};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

pub(crate) enum EntrypointKind {
    HttpRoute,
    EventListener,
    Scheduled,
    Main,
    Fanout,
}

impl EntrypointKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            EntrypointKind::HttpRoute => "http_route",
            EntrypointKind::EventListener => "event_listener",
            EntrypointKind::Scheduled => "scheduled",
            EntrypointKind::Main => "main",
            EntrypointKind::Fanout => "fanout",
        }
    }

    pub(crate) fn business_flow(&self) -> bool {
        matches!(
            self,
            EntrypointKind::HttpRoute | EntrypointKind::EventListener
        )
    }

    pub(crate) fn business_surface(&self) -> &'static str {
        match self {
            EntrypointKind::HttpRoute => "http",
            EntrypointKind::EventListener => "event",
            EntrypointKind::Scheduled => "scheduled",
            EntrypointKind::Main => "main",
            EntrypointKind::Fanout => "internal",
        }
    }
}

pub(crate) struct ScoredEntrypoint {
    pub id: NodeId,
    pub score: f64,
    pub kind: EntrypointKind,
    pub route_method: Option<String>,
    pub route_path: Option<String>,
    pub event_topics: Vec<String>,
}

struct RouteInfo {
    method: String,
    path: String,
}

pub fn score_entry_points(
    nodes: &[Node],
    edges: &[Edge],
    digraph: &DiGraph<NodeId, f32>,
    node_index: &HashMap<NodeId, NodeIndex>,
) -> Vec<ScoredEntrypoint> {
    let by_id: HashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();

    // Pre-index HandlesRoute edges: handler_id → RouteInfo
    let mut route_edges: HashMap<NodeId, RouteInfo> = HashMap::new();
    // Pre-index ListensTo edges: listener_id → sorted Vec<topic_name>
    let mut listens_edges: HashMap<NodeId, Vec<String>> = HashMap::new();

    for e in edges {
        match e.kind {
            EdgeKind::HandlesRoute => {
                if let Some(route_node) = by_id.get(&e.dst) {
                    let method = route_node
                        .props
                        .as_ref()
                        .and_then(|p| p.get("httpMethod"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| route_node.name.split(' ').next().unwrap_or("GET"))
                        .to_string();
                    let path = route_node
                        .props
                        .as_ref()
                        .and_then(|p| p.get("path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            route_node
                                .name
                                .splitn(2, ' ')
                                .nth(1)
                                .unwrap_or(&route_node.name)
                        })
                        .to_string();
                    route_edges.insert(e.src.clone(), RouteInfo { method, path });
                }
            }
            EdgeKind::ListensTo => {
                let topic_name = by_id
                    .get(&e.dst)
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| {
                        e.dst
                            .as_str()
                            .strip_prefix("KafkaTopic:")
                            .unwrap_or(e.dst.as_str())
                            .to_string()
                    });
                listens_edges
                    .entry(e.src.clone())
                    .or_default()
                    .push(topic_name);
            }
            _ => {}
        }
    }

    // Sort topic lists for determinism
    for topics in listens_edges.values_mut() {
        let set: BTreeSet<String> = topics.drain(..).collect();
        *topics = set.into_iter().collect();
    }

    let mut scored = Vec::new();

    for node in nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Method | NodeKind::Constructor))
    {
        if is_test_method(node, &by_id) {
            continue;
        }

        let Some(&idx) = node_index.get(&node.id) else {
            continue;
        };

        let name = by_id
            .get(&node.id)
            .map(|n| n.name.as_str())
            .unwrap_or(node.id.as_str());

        let callees = digraph.neighbors_directed(idx, Direction::Outgoing).count() as f64;
        let callers = digraph.neighbors_directed(idx, Direction::Incoming).count() as f64;

        // Classify by edge evidence first
        let (kind, route_method, route_path, event_topics) =
            if let Some(ri) = route_edges.get(&node.id) {
                (
                    EntrypointKind::HttpRoute,
                    Some(ri.method.clone()),
                    Some(ri.path.clone()),
                    Vec::new(),
                )
            } else if let Some(topics) = listens_edges.get(&node.id) {
                (EntrypointKind::EventListener, None, None, topics.clone())
            } else if name == "main" {
                (EntrypointKind::Main, None, None, Vec::new())
            } else if is_scheduled_name(name) {
                (EntrypointKind::Scheduled, None, None, Vec::new())
            } else {
                (EntrypointKind::Fanout, None, None, Vec::new())
            };

        // Skip zero-callee methods except HttpRoute and EventListener
        if callees == 0.0 {
            match kind {
                EntrypointKind::HttpRoute | EntrypointKind::EventListener => {}
                _ => continue,
            }
        }

        let multiplier = match kind {
            EntrypointKind::HttpRoute | EntrypointKind::EventListener => 3.0,
            EntrypointKind::Scheduled | EntrypointKind::Main => 2.0,
            EntrypointKind::Fanout => {
                if is_utility_name(name) {
                    0.3
                } else if is_entry_name(name) {
                    1.5
                } else {
                    1.0
                }
            }
        };

        let score = (callees / (callers + 1.0)) * multiplier;
        scored.push(ScoredEntrypoint {
            id: node.id.clone(),
            score,
            kind,
            route_method,
            route_path,
            event_topics,
        });
    }

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    scored.truncate(200);
    scored
}

fn is_test_method(node: &Node, by_id: &HashMap<&NodeId, &Node>) -> bool {
    // File path
    if node.file.contains("/test/") || node.file.contains("/src/test/") {
        return true;
    }
    // isTest prop
    if node
        .props
        .as_ref()
        .and_then(|p| p.get("isTest"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    // Method name suffix
    let name = &node.name;
    if name.ends_with("Test")
        || name.ends_with("Tests")
        || name.ends_with("IT")
        || name.ends_with("Spec")
    {
        return true;
    }
    // Enclosing class from ID
    let class_name = extract_class_name(node.id.as_str());
    if let Some(cn) = &class_name {
        if cn.ends_with("Test")
            || cn.ends_with("Tests")
            || cn.ends_with("IT")
            || cn.ends_with("Spec")
        {
            return true;
        }
        // Check class node stereotype
        let class_id_candidates = [
            format!("Class:{}", cn),
            format!(
                "Class:{}",
                node.id
                    .as_str()
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim_start_matches("Method:")
                    .trim_start_matches("Constructor:")
            ),
        ];
        for cid in &class_id_candidates {
            let key = NodeId::new(cid.clone());
            if let Some(class_node) = by_id.get(&key) {
                if class_node
                    .props
                    .as_ref()
                    .and_then(|p| p.get("stereotype"))
                    .and_then(|v| v.as_str())
                    == Some("test")
                {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_class_name(id: &str) -> Option<String> {
    let without_kind = id
        .strip_prefix("Method:")
        .or_else(|| id.strip_prefix("Constructor:"))?;
    let fqcn = without_kind.split('#').next()?;
    Some(fqcn.rsplit('.').next()?.to_string())
}

fn is_scheduled_name(name: &str) -> bool {
    const SCHEDULED_PREFIXES: &[&str] = &["run", "execute", "schedule", "batch"];
    SCHEDULED_PREFIXES
        .iter()
        .any(|p| starts_word_boundary(name, p))
}

fn starts_word_boundary(name: &str, prefix: &str) -> bool {
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

fn is_entry_name(name: &str) -> bool {
    if name == "main" {
        return true;
    }
    const STARTS: &[&str] = &[
        "main", "init", "execute", "run", "start", "handle", "process", "perform", "dispatch",
        "trigger", "fire", "emit",
    ];
    const ENDS: &[&str] = &["Handler", "Controller", "Listener", "Endpoint"];
    STARTS.iter().any(|p| starts_word_boundary(name, p)) || ENDS.iter().any(|s| name.ends_with(s))
}

fn is_utility_name(name: &str) -> bool {
    const STARTS: &[&str] = &[
        "get", "set", "is", "has", "to", "from", "format", "parse", "validate", "convert", "log",
        "debug",
    ];
    const ENDS: &[&str] = &["Helper", "Util", "Utils"];
    STARTS.iter().any(|p| name.starts_with(p)) || ENDS.iter().any(|s| name.ends_with(s))
}

pub(crate) fn to_legacy_pairs(scored: &[ScoredEntrypoint]) -> Vec<(NodeId, f64)> {
    scored.iter().map(|s| (s.id.clone(), s.score)).collect()
}
