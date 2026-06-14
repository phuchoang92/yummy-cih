use cih_core::{Node, NodeKind};

pub fn embeddable_nodes<'a>(nodes: &'a [Node]) -> Vec<&'a Node> {
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
    )
}

pub fn embedding_text(node: &Node) -> String {
    let mut parts = Vec::new();
    parts.push(format!("kind: {}", kind_label(node.kind)));
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
    parts.join("\n")
}

pub fn content_hash(node_id: &str, chunk_text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(node_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(chunk_text.as_bytes());
    hasher.finalize().to_hex()[..32].to_string()
}

fn kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "File",
        NodeKind::Folder => "Folder",
        NodeKind::Class => "Class",
        NodeKind::Interface => "Interface",
        NodeKind::Enum => "Enum",
        NodeKind::Record => "Record",
        NodeKind::Annotation => "Annotation",
        NodeKind::Method => "Method",
        NodeKind::Function => "Function",
        NodeKind::Constructor => "Constructor",
        NodeKind::Field => "Field",
        NodeKind::Route => "Route",
        NodeKind::Community => "Community",
        NodeKind::Process => "Process",
        NodeKind::Other => "Other",
    }
}
