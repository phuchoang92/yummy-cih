use cih_core::{file_id, ParsedUnit, RawImport};
use tree_sitter::Node as TsNode;
use crate::generic_parse::{emit_function, emit_method, emit_type, range_of, text, build_unit};
use cih_core::{Edge, Node, NodeKind, SymbolDef, ReferenceSite};

fn namespace_of(root: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "namespace_definition" {
            if let Some(n) = child.child_by_field_name("name") {
                return Some(text(n, src).replace('\\', "."));
            }
        }
    }
    None
}

pub fn parse_php_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser.parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let ns = namespace_of(root, src).unwrap_or_default();
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut imports: Vec<RawImport> = Vec::new();
    let mut sites: Vec<ReferenceSite> = Vec::new();

    walk(root, src, rel, &ns, &file_node_id, None,
         &mut nodes, &mut edges, &mut defs, &mut imports, &mut sites);

    Ok(build_unit(rel, "php", if ns.is_empty() { None } else { Some(ns) },
                  nodes, edges, defs, imports, sites))
}

#[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)] // walker signature; `sites` reserved for reference-site collection
fn walk(
    parent: TsNode<'_>, src: &str, rel: &str, ns: &str,
    file_node_id: &cih_core::NodeId, owner_fqcn: Option<&str>,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
    imports: &mut Vec<RawImport>, sites: &mut Vec<ReferenceSite>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "namespace_use_declaration" => {
                let mut ic = child.walk();
                for clause in child.named_children(&mut ic) {
                    if clause.kind() == "namespace_use_clause" {
                        if let Some(n) = clause.child_by_field_name("name") {
                            let raw = text(n, src).replace('\\', ".");
                            imports.push(RawImport { raw, is_static: false, is_wildcard: false, range: range_of(clause) });
                        }
                    }
                }
            }
            "class_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let fqcn = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
                    emit_type(NodeKind::Class, &name, &fqcn, range_of(child), rel, file_node_id, nodes, edges, defs);
                    walk(child, src, rel, ns, file_node_id, Some(&fqcn),
                         nodes, edges, defs, imports, sites);
                }
            }
            "interface_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let fqcn = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
                    emit_type(NodeKind::Interface, &name, &fqcn, range_of(child), rel, file_node_id, nodes, edges, defs);
                }
            }
            "function_definition" | "method_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let arity = child.child_by_field_name("parameters")
                        .map(|p| p.named_child_count() as u16).unwrap_or(0);
                    if let Some(owner) = owner_fqcn {
                        emit_method(&name, owner, arity, range_of(child), rel, nodes, edges, defs);
                    } else {
                        let fqcn = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
                        emit_function(&name, &fqcn, arity, range_of(child), rel, file_node_id, nodes, edges, defs);
                    }
                }
            }
            "namespace_definition" => {
                walk(child, src, rel, ns, file_node_id, owner_fqcn,
                     nodes, edges, defs, imports, sites);
            }
            _ => {}
        }
    }
}
