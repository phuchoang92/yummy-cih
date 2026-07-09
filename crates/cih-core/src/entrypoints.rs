use rustc_hash::FxHashMap;
use std::collections::BTreeSet;
use std::path::Path;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use crate::{Edge, EdgeKind, Node, NodeId, NodeKind};

// ── Registry ──────────────────────────────────────────────────────────────────

/// Annotation/decorator patterns that identify entry-point methods across languages.
///
/// Defaults cover Java (Spring MVC, JAX-RS, Kafka), TypeScript (NestJS), and Python
/// (Flask, FastAPI, Celery). Per-project overrides go in `.cih/entry_points/*.toml`:
///
/// ```toml
/// [http]
/// annotations = ["MyRoute", ...]
///
/// [event]
/// annotations = ["MyListener", ...]
///
/// [scheduled]
/// annotations = ["MyCron", ...]
/// ```
#[derive(Debug, Default, Clone)]
pub struct EntrypointRegistry {
    pub(crate) http: BTreeSet<String>,
    pub(crate) event: BTreeSet<String>,
    pub(crate) scheduled: BTreeSet<String>,
}

impl EntrypointRegistry {
    /// Build defaults and merge any `{repo}/.cih/entry_points/*.toml` overrides.
    pub fn load(repo: &Path) -> Self {
        let mut reg = Self::builtin_defaults();
        let override_dir = repo.join(".cih").join("entry_points");
        if override_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&override_dir) {
                let mut paths: Vec<_> = entries
                    .flatten()
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("toml"))
                    .map(|e| e.path())
                    .collect();
                paths.sort();
                for path in paths {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        tracing::debug!(path = %path.display(), "loading entry_points override");
                        reg.merge_toml(&content);
                    }
                }
            }
        }
        reg
    }

    fn builtin_defaults() -> Self {
        let mut reg = Self::default();
        Self::java_defaults(&mut reg);
        Self::typescript_defaults(&mut reg);
        Self::python_defaults(&mut reg);
        reg
    }

    fn java_defaults(reg: &mut Self) {
        for ann in [
            "GetMapping",
            "PostMapping",
            "PutMapping",
            "DeleteMapping",
            "PatchMapping",
            "RequestMapping",
            "GET",
            "POST",
            "PUT",
            "DELETE",
            "PATCH",
            "HEAD",
            "OPTIONS",
        ] {
            reg.http.insert(ann.to_string());
        }
        for ann in [
            "KafkaListener",
            "EventListener",
            "RabbitListener",
            "JmsListener",
            "SqsListener",
            "StreamListener",
        ] {
            reg.event.insert(ann.to_string());
        }
        for ann in ["Scheduled", "Cron"] {
            reg.scheduled.insert(ann.to_string());
        }
    }

    fn typescript_defaults(reg: &mut Self) {
        for ann in [
            "Get", "Post", "Put", "Delete", "Patch", "Head", "Options", "All",
        ] {
            reg.http.insert(ann.to_string());
        }
        for ann in ["MessagePattern", "EventPattern"] {
            reg.event.insert(ann.to_string());
        }
        for ann in ["Cron", "Interval", "Timeout"] {
            reg.scheduled.insert(ann.to_string());
        }
    }

    fn python_defaults(reg: &mut Self) {
        for ann in [
            "app.route",
            "app.get",
            "app.post",
            "app.put",
            "app.delete",
            "app.patch",
            "router.get",
            "router.post",
            "router.put",
            "router.delete",
            "router.patch",
            "blueprint.route",
        ] {
            reg.http.insert(ann.to_string());
        }
        for ann in ["task", "app.task", "shared_task", "celery.task"] {
            reg.event.insert(ann.to_string());
        }
    }

    fn merge_toml(&mut self, content: &str) {
        let Ok(table) = content.parse::<toml::Table>() else {
            return;
        };
        for (section, target) in [
            ("http", &mut self.http),
            ("event", &mut self.event),
            ("scheduled", &mut self.scheduled),
        ] {
            if let Some(anns) = table
                .get(section)
                .and_then(|v| v.as_table())
                .and_then(|t| t.get("annotations"))
                .and_then(|v| v.as_array())
            {
                for ann in anns.iter().filter_map(|v| v.as_str()) {
                    target.insert(ann.to_string());
                }
            }
        }
    }

    pub fn http_annotations(&self) -> &BTreeSet<String> {
        &self.http
    }

    pub fn event_annotations(&self) -> &BTreeSet<String> {
        &self.event
    }

    pub fn scheduled_annotations(&self) -> &BTreeSet<String> {
        &self.scheduled
    }

    pub fn total_patterns(&self) -> usize {
        self.http.len() + self.event.len() + self.scheduled.len()
    }
}

// ── Entrypoint kinds & scored results ─────────────────────────────────────────

pub enum EntrypointKind {
    HttpRoute,
    EventListener,
    Scheduled,
    Main,
    Fanout,
}

impl EntrypointKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntrypointKind::HttpRoute => "http_route",
            EntrypointKind::EventListener => "event_listener",
            EntrypointKind::Scheduled => "scheduled",
            EntrypointKind::Main => "main",
            EntrypointKind::Fanout => "fanout",
        }
    }

    pub fn business_flow(&self) -> bool {
        matches!(
            self,
            EntrypointKind::HttpRoute | EntrypointKind::EventListener
        )
    }

    pub fn business_surface(&self) -> &'static str {
        match self {
            EntrypointKind::HttpRoute => "http",
            EntrypointKind::EventListener => "event",
            EntrypointKind::Scheduled => "scheduled",
            EntrypointKind::Main => "main",
            EntrypointKind::Fanout => "internal",
        }
    }
}

pub struct ScoredEntrypoint {
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

// ── Graph utility ─────────────────────────────────────────────────────────────

/// Builds a directed calls graph (Method/Constructor/Function nodes only).
pub fn build_calls_digraph(
    nodes: &[Node],
    edges: &[Edge],
    min_confidence: f32,
) -> (DiGraph<NodeId, f32>, FxHashMap<NodeId, NodeIndex>) {
    let mut graph = DiGraph::<NodeId, f32>::new();
    let mut index = FxHashMap::default();
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

fn is_callable(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Method | NodeKind::Constructor | NodeKind::Function
    )
}

// ── Scoring ───────────────────────────────────────────────────────────────────

pub fn score_entry_points(
    nodes: &[Node],
    edges: &[Edge],
    digraph: &DiGraph<NodeId, f32>,
    node_index: &FxHashMap<NodeId, NodeIndex>,
    registry: &EntrypointRegistry,
) -> Vec<ScoredEntrypoint> {
    let by_id: FxHashMap<&NodeId, &Node> = nodes.iter().map(|n| (&n.id, n)).collect();

    let mut route_edges: FxHashMap<NodeId, RouteInfo> = FxHashMap::default();
    let mut listens_edges: FxHashMap<NodeId, Vec<String>> = FxHashMap::default();

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
                                .split_once(' ')
                                .map(|x| x.1)
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

    for topics in listens_edges.values_mut() {
        let set: BTreeSet<String> = topics.drain(..).collect();
        *topics = set.into_iter().collect();
    }

    let mut scored = Vec::new();

    for node in nodes.iter().filter(|n| {
        matches!(
            n.kind,
            NodeKind::Method | NodeKind::Constructor | NodeKind::Function
        )
    }) {
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
            } else if node_annotation_matches(node, registry.http_annotations()) {
                let method = prop_str(node, "httpMethod").unwrap_or("GET").to_string();
                let path = prop_str(node, "path")
                    .unwrap_or_else(|| node.name.split_once(' ').map(|x| x.1).unwrap_or(""))
                    .to_string();
                (
                    EntrypointKind::HttpRoute,
                    Some(method),
                    Some(path),
                    Vec::new(),
                )
            } else if node_annotation_matches(node, registry.event_annotations()) {
                (EntrypointKind::EventListener, None, None, Vec::new())
            } else if node_annotation_matches(node, registry.scheduled_annotations())
                || is_scheduled_name(name)
            {
                (EntrypointKind::Scheduled, None, None, Vec::new())
            } else if name == "main" {
                (EntrypointKind::Main, None, None, Vec::new())
            } else {
                (EntrypointKind::Fanout, None, None, Vec::new())
            };

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

/// Convenience: build the calls digraph and score in one call.
pub fn score_all_entry_points(
    nodes: &[Node],
    edges: &[Edge],
    min_confidence: f32,
    registry: &EntrypointRegistry,
) -> Vec<ScoredEntrypoint> {
    let (digraph, node_index) = build_calls_digraph(nodes, edges, min_confidence);
    if digraph.node_count() == 0 {
        return Vec::new();
    }
    score_entry_points(nodes, edges, &digraph, &node_index, registry)
}

pub fn to_legacy_pairs(scored: &[ScoredEntrypoint]) -> Vec<(NodeId, f64)> {
    scored.iter().map(|s| (s.id.clone(), s.score)).collect()
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn is_test_method(node: &Node, by_id: &FxHashMap<&NodeId, &Node>) -> bool {
    if node.file.contains("/test/") || node.file.contains("/src/test/") {
        return true;
    }
    if node
        .props
        .as_ref()
        .and_then(|p| p.get("isTest"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let name = &node.name;
    if name.ends_with("Test")
        || name.ends_with("Tests")
        || name.ends_with("IT")
        || name.ends_with("Spec")
    {
        return true;
    }
    let class_name = extract_class_name(node.id.as_str());
    if let Some(cn) = &class_name {
        if cn.ends_with("Test")
            || cn.ends_with("Tests")
            || cn.ends_with("IT")
            || cn.ends_with("Spec")
        {
            return true;
        }
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

fn node_annotation_matches(node: &Node, set: &BTreeSet<String>) -> bool {
    let Some(props) = node.props.as_ref() else {
        return false;
    };
    let Some(anns) = props.get("route_annotations").and_then(|v| v.as_array()) else {
        return false;
    };
    anns.iter()
        .filter_map(|v| v.as_str())
        .any(|a| set.contains(a))
}

fn prop_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.as_ref()?.get(key)?.as_str()
}

#[cfg(test)]
#[path = "entrypoints_tests.rs"]
mod tests;
