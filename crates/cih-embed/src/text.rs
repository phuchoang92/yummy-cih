use cih_core::{Node, NodeKind};

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

pub fn embedding_text(node: &Node) -> String {
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
