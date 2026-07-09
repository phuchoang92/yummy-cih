//! Shared AST-walking utilities used by simple language providers.
//! Each language provider calls the helpers relevant to its grammar node names.

use cih_core::{
    function_id, type_id, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit,
    Range, RawImport, RefKind, ReferenceSite, SymbolDef,
};
use tree_sitter::Node as TsNode;

pub fn range_of(node: TsNode<'_>) -> Range {
    let s = node.start_position();
    let e = node.end_position();
    Range {
        start_line: s.row as u32 + 1,
        start_col: s.column as u32,
        end_line: e.row as u32 + 1,
        end_col: e.column as u32,
    }
}

pub fn text<'a>(node: TsNode<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("").trim()
}

/// Emit a Function node + Contains edge and SymbolDef.
#[allow(clippy::too_many_arguments)] // flat emitter signature shared by all providers
pub fn emit_function(
    name: &str, fqcn: &str, arity: u16, range: Range, rel: &str,
    file_node_id: &NodeId,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
) {
    let id = function_id(fqcn, name, arity);
    defs.push(SymbolDef {
        id: id.clone(), kind: NodeKind::Function, fqcn: fqcn.to_string(),
        name: name.to_string(), owner: None, range, modifiers: Vec::new(),
        param_types: Vec::new(), return_type: None, declared_type: None,
        framework_role: None, complexity: None, body_fingerprint: None,
    lang_meta: None,
    });
    nodes.push(Node {
        id: id.clone(), kind: NodeKind::Function, name: name.to_string(),
        qualified_name: Some(fqcn.to_string()), file: rel.to_string(), range, props: None,
    });
    edges.push(Edge {
        src: file_node_id.clone(), dst: id, kind: EdgeKind::Contains,
        confidence: 1.0, reason: "structure".into(), props: None,
    });
}

/// Emit a Class/Interface/Enum node + Contains edge and SymbolDef.
#[allow(clippy::too_many_arguments)] // flat emitter signature shared by all providers
pub fn emit_type(
    kind: NodeKind, name: &str, fqcn: &str, range: Range, rel: &str,
    file_node_id: &NodeId,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
) {
    let id = type_id(kind, fqcn);
    defs.push(SymbolDef {
        id: id.clone(), kind, fqcn: fqcn.to_string(),
        name: name.to_string(), owner: None, range, modifiers: Vec::new(),
        param_types: Vec::new(), return_type: None, declared_type: None,
        framework_role: None, complexity: None, body_fingerprint: None,
    lang_meta: None,
    });
    nodes.push(Node {
        id: id.clone(), kind, name: name.to_string(),
        qualified_name: Some(fqcn.to_string()), file: rel.to_string(), range, props: None,
    });
    edges.push(Edge {
        src: file_node_id.clone(), dst: id, kind: EdgeKind::Contains,
        confidence: 1.0, reason: "structure".into(), props: None,
    });
}

/// Emit a Method node + HAS_METHOD edge from an owner type.
#[allow(clippy::too_many_arguments)] // flat emitter signature shared by all providers
pub fn emit_method(
    name: &str, owner_fqcn: &str, arity: u16, range: Range, rel: &str,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
) {
    let owner_id = type_id(NodeKind::Class, owner_fqcn);
    let id = cih_core::method_id(owner_fqcn, name, arity);
    defs.push(SymbolDef {
        id: id.clone(), kind: NodeKind::Method, fqcn: owner_fqcn.to_string(),
        name: name.to_string(), owner: Some(owner_id.clone()), range,
        modifiers: Vec::new(), param_types: Vec::new(), return_type: None,
        declared_type: None, framework_role: None, complexity: None, body_fingerprint: None,
    lang_meta: None,
    });
    nodes.push(Node {
        id: id.clone(), kind: NodeKind::Method, name: name.to_string(),
        qualified_name: Some(format!("{owner_fqcn}.{name}")), file: rel.to_string(), range, props: None,
    });
    edges.push(Edge {
        src: owner_id, dst: id, kind: EdgeKind::HasMethod,
        confidence: 1.0, reason: "structure".into(), props: None,
    });
}

/// Simple call-site collector: walks any subtree for call_expression nodes.
#[allow(clippy::too_many_arguments)] // flat emitter signature shared by all providers
pub fn collect_calls_generic(
    root: TsNode<'_>, src: &str, call_kind: &str,
    function_field: &str, receiver_field: Option<&str>, name_field: Option<&str>,
    in_fqcn: &str, in_callable: &NodeId, sites: &mut Vec<ReferenceSite>,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == call_kind {
            let func = node.child_by_field_name(function_field);
            if let Some(func_node) = func {
                let (receiver, name) = if let Some(rf) = receiver_field {
                    let recv = func_node.child_by_field_name(rf)
                        .map(|n| text(n, src).to_string());
                    let nm = name_field
                        .and_then(|nf| func_node.child_by_field_name(nf))
                        .map(|n| text(n, src).to_string())
                        .unwrap_or_else(|| text(func_node, src).to_string());
                    (recv, nm)
                } else {
                    (None, text(func_node, src).to_string())
                };
                if !name.is_empty() {
                    sites.push(ReferenceSite {
                        name, receiver, kind: RefKind::Call, arity: Some(0),
                        range: range_of(node), in_fqcn: in_fqcn.to_string(),
                        in_callable: in_callable.clone(), arg_texts: Vec::new(),
                    });
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) { stack.push(child); }
    }
}

/// Build a minimal ParsedUnit for languages that only extract top-level defs.
#[allow(clippy::too_many_arguments)] // flat emitter signature shared by all providers
pub fn build_unit(
    rel: &str, language: &str, package: Option<String>,
    nodes: Vec<Node>, edges: Vec<Edge>, defs: Vec<SymbolDef>,
    imports: Vec<RawImport>, reference_sites: Vec<ReferenceSite>,
) -> ParsedUnit {
    let parsed_file = ParsedFile {
        file: rel.to_string(),
        language: language.to_string(),
        package,
        defs,
        imports,
        reference_sites,
        ..Default::default()
    };
    ParsedUnit { rel: rel.to_string(), nodes, edges, parsed_file, import_bindings: Vec::new() }
}
