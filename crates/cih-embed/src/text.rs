use std::collections::HashMap;
use std::path::Path;

use cih_core::{Node, NodeKind};

use crate::strip::strip_java_body;

pub fn embeddable_nodes(nodes: &[Node]) -> Vec<&Node> {
    nodes
        .iter()
        .filter(|node| is_embeddable_kind(node.kind))
        .collect()
}

pub fn is_embeddable_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Record
            | NodeKind::Annotation
            | NodeKind::Method
            | NodeKind::Constructor
            | NodeKind::Field
            | NodeKind::Route
            | NodeKind::IntegrationRoute
    )
}

/// Kinds for which we extract and strip source body text.
fn is_body_kind(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Method | NodeKind::Constructor | NodeKind::Function)
}

fn file_ext(file: &str) -> &str {
    file.rfind('.').map(|i| &file[i + 1..]).unwrap_or("")
}

/// Build a node_id → stripped body map by reading source files from `repo`.
/// Only Method/Constructor/Function nodes with valid line ranges get a body entry.
pub fn source_bodies(nodes: &[Node], repo: &Path) -> HashMap<String, String> {
    let mut file_lines: HashMap<String, Vec<String>> = HashMap::new();
    let mut bodies: HashMap<String, String> = HashMap::new();

    for node in nodes {
        if !is_body_kind(node.kind) {
            continue;
        }
        let start = node.range.start_line as usize;
        let end = node.range.end_line as usize;
        if start == 0 && end == 0 {
            continue;
        }

        let lines = file_lines.entry(node.file.clone()).or_insert_with(|| {
            std::fs::read_to_string(repo.join(&node.file))
                .unwrap_or_default()
                .lines()
                .map(|l| l.to_string())
                .collect()
        });
        if lines.is_empty() {
            continue;
        }

        let from = start.saturating_sub(1);
        let to = end.min(lines.len());
        if from >= to {
            continue;
        }

        let raw = lines[from..to].join("\n");
        let stripped = match file_ext(&node.file) {
            "java" => strip_java_body(&raw),
            _ => raw,
        };

        if !stripped.trim().is_empty() {
            bodies.insert(node.id.as_str().to_string(), stripped);
        }
    }

    bodies
}

pub fn embedding_text(node: &Node, body: Option<&str>) -> String {
    let mut parts = Vec::new();
    parts.push(format!("kind: {}", node.kind.label()));
    parts.push(format!("name: {}", node.name));
    if let Some(qualified_name) = &node.qualified_name {
        parts.push(format!("qualified_name: {qualified_name}"));
    }
    parts.push(format!("file: {}", node.file));
    if node.range.start_line > 0 || node.range.end_line > 0 {
        parts.push(format!(
            "lines: {}-{}",
            node.range.start_line, node.range.end_line
        ));
    }
    // Enrich route nodes with HTTP method and path for better semantic matching.
    if let Some(props) = &node.props {
        if matches!(node.kind, NodeKind::Route) {
            if let Some(method) = props.get("httpMethod").and_then(|v| v.as_str()) {
                parts.push(format!("http_method: {method}"));
            }
            if let Some(path) = props.get("path").and_then(|v| v.as_str()) {
                parts.push(format!("path: {path}"));
            }
        }
        if matches!(node.kind, NodeKind::IntegrationRoute) {
            if let Some(uri) = props.get("uri").and_then(|v| v.as_str()) {
                parts.push(format!("uri: {uri}"));
            }
            if let Some(source) = props.get("source").and_then(|v| v.as_str()) {
                parts.push(format!("source: {source}"));
            }
        }
    }
    if let Some(b) = body {
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            parts.push(format!("---\n{trimmed}"));
        }
    }
    parts.join("\n")
}

pub fn content_hash(node_id: &str, chunk_text: &str) -> String {
    // 128 bits is plenty for change detection here and keeps row keys/logs compact.
    let mut hasher = blake3::Hasher::new();
    hasher.update(node_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(chunk_text.as_bytes());
    hasher.finalize().to_hex()[..32].to_string()
}
