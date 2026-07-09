use cih_core::{file_id, ParsedUnit, RawImport};
use tree_sitter::Node as TsNode;
use crate::generic_parse::{emit_function, emit_method, emit_type, range_of, text, build_unit};
use cih_core::{Edge, Node, NodeKind, SymbolDef, ReferenceSite};

pub fn parse_cpp_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser.parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut imports: Vec<RawImport> = Vec::new();
    let mut sites: Vec<ReferenceSite> = Vec::new();

    walk(root, src, rel, &file_node_id, None,
         &mut nodes, &mut edges, &mut defs, &mut imports, &mut sites);

    Ok(build_unit(rel, "cpp", None, nodes, edges, defs, imports, sites))
}

#[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)] // walker signature; `sites` reserved for reference-site collection
fn walk(
    parent: TsNode<'_>, src: &str, rel: &str,
    file_node_id: &cih_core::NodeId, owner_fqcn: Option<&str>,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
    imports: &mut Vec<RawImport>, sites: &mut Vec<ReferenceSite>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "preproc_include" => {
                if let Some(path_node) = child.child_by_field_name("path") {
                    let raw = text(path_node, src).trim_matches('"').trim_matches('<').trim_matches('>').to_string();
                    if !raw.is_empty() {
                        imports.push(RawImport { raw, is_static: false, is_wildcard: false, range: range_of(child) });
                    }
                }
            }
            "class_specifier" | "struct_specifier" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    emit_type(NodeKind::Class, &name, &name, range_of(child), rel, file_node_id, nodes, edges, defs);
                    walk(child, src, rel, file_node_id, Some(&name),
                         nodes, edges, defs, imports, sites);
                }
            }
            "function_definition" => {
                let decl = child.child_by_field_name("declarator");
                if let Some(name) = extract_cpp_func_name(decl, src) {
                    let arity = 0u16; // simplified
                    if let Some(owner) = owner_fqcn {
                        emit_method(&name, owner, arity, range_of(child), rel, nodes, edges, defs);
                    } else {
                        emit_function(&name, &name, arity, range_of(child), rel, file_node_id, nodes, edges, defs);
                    }
                }
            }
            "namespace_definition" => {
                walk(child, src, rel, file_node_id, owner_fqcn,
                     nodes, edges, defs, imports, sites);
            }
            _ => {}
        }
    }
}

fn extract_cpp_func_name(declarator: Option<TsNode<'_>>, src: &str) -> Option<String> {
    let decl = declarator?;
    // function_declarator → declarator → qualified_identifier or identifier
    let inner = decl.child_by_field_name("declarator").unwrap_or(decl);
    match inner.kind() {
        "qualified_identifier" => {
            Some(text(inner, src).to_string())
        }
        "identifier" | "field_identifier" => {
            Some(text(inner, src).to_string())
        }
        _ => Some(text(inner, src).to_string()),
    }
}
