use cih_core::{file_id, ParsedUnit, RawImport};
use tree_sitter::Node as TsNode;
use crate::generic_parse::{collect_calls_generic, emit_function, emit_method, emit_type, range_of, text, build_unit};
use cih_core::{Edge, Node, NodeKind, SymbolDef, ReferenceSite};

fn namespace_of(root: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "namespace_declaration" || child.kind() == "file_scoped_namespace_declaration" {
            if let Some(name_node) = child.child_by_field_name("name") {
                return Some(text(name_node, src).to_string());
            }
        }
    }
    None
}

pub fn parse_csharp_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
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

    Ok(build_unit(rel, "csharp", if ns.is_empty() { None } else { Some(ns) },
                  nodes, edges, defs, imports, sites))
}

fn walk(
    parent: TsNode<'_>, src: &str, rel: &str, ns: &str,
    file_node_id: &cih_core::NodeId, owner_fqcn: Option<&str>,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
    imports: &mut Vec<RawImport>, sites: &mut Vec<ReferenceSite>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "using_directive" => {
                if let Some(n) = child.child_by_field_name("name") {
                    let raw = text(n, src).to_string();
                    let is_wildcard = raw.ends_with(".*");
                    imports.push(cih_core::RawImport {
                        raw: raw.trim_end_matches(".*").to_string(),
                        is_static: false, is_wildcard,
                        range: range_of(child),
                    });
                }
            }
            "class_declaration" | "record_declaration" => {
                let kind = if child.kind() == "record_declaration" { NodeKind::Record } else { NodeKind::Class };
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let fqcn = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
                    emit_type(kind, &name, &fqcn, range_of(child), rel, file_node_id, nodes, edges, defs);
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
            "enum_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let fqcn = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
                    emit_type(NodeKind::Enum, &name, &fqcn, range_of(child), rel, file_node_id, nodes, edges, defs);
                }
            }
            "method_declaration" | "constructor_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let arity = child.child_by_field_name("parameters")
                        .map(|p| p.named_child_count() as u16).unwrap_or(0);
                    if let Some(owner) = owner_fqcn {
                        let callable_id = cih_core::method_id(owner, &name, arity);
                        collect_calls_generic(child, src, "invocation_expression", "function",
                            None, None, owner, &callable_id, sites);
                        emit_method(&name, owner, arity, range_of(child), rel, nodes, edges, defs);
                    } else {
                        let fqcn = if ns.is_empty() { name.clone() } else { format!("{ns}.{name}") };
                        let callable_id = cih_core::function_id(&fqcn, &name, arity);
                        collect_calls_generic(child, src, "invocation_expression", "function",
                            None, None, &fqcn, &callable_id, sites);
                        emit_function(&name, &fqcn, arity, range_of(child), rel, file_node_id, nodes, edges, defs);
                    }
                }
            }
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                walk(child, src, rel, ns, file_node_id, owner_fqcn,
                     nodes, edges, defs, imports, sites);
            }
            _ => {}
        }
    }
}
