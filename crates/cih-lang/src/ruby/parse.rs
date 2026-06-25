use cih_core::{file_id, ParsedUnit, RawImport};
use tree_sitter::Node as TsNode;
use crate::generic_parse::{emit_function, emit_method, emit_type, range_of, text, build_unit};
use cih_core::{Edge, Node, NodeKind, SymbolDef, ReferenceSite};

pub fn parse_ruby_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
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

    walk(root, src, rel, &file_node_id, None, &mut nodes, &mut edges, &mut defs, &mut imports, &mut sites);

    Ok(build_unit(rel, "ruby", None, nodes, edges, defs, imports, sites))
}

fn walk(
    parent: TsNode<'_>, src: &str, rel: &str,
    file_node_id: &cih_core::NodeId, owner_fqcn: Option<&str>,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
    imports: &mut Vec<RawImport>, sites: &mut Vec<ReferenceSite>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "call" => {
                // `require 'foo'` or `require_relative 'bar'`
                let method = child.child_by_field_name("method");
                if let Some(m) = method {
                    let mn = text(m, src);
                    if mn == "require" || mn == "require_relative" {
                        if let Some(args) = child.child_by_field_name("arguments") {
                            let mut ac = args.walk();
                            for arg in args.named_children(&mut ac) {
                                let raw = text(arg, src).trim_matches('"').trim_matches('\'').to_string();
                                if !raw.is_empty() {
                                    imports.push(RawImport { raw, is_static: false, is_wildcard: false, range: range_of(child) });
                                }
                            }
                        }
                    }
                }
            }
            "class" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    emit_type(NodeKind::Class, &name, &name, range_of(child), rel, file_node_id, nodes, edges, defs);
                    walk(child, src, rel, file_node_id, Some(&name), nodes, edges, defs, imports, sites);
                }
            }
            "module" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    emit_type(NodeKind::Class, &name, &name, range_of(child), rel, file_node_id, nodes, edges, defs);
                    walk(child, src, rel, file_node_id, Some(&name), nodes, edges, defs, imports, sites);
                }
            }
            "method" | "singleton_method" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let arity = child.child_by_field_name("parameters")
                        .map(|p| p.named_child_count() as u16).unwrap_or(0);
                    if let Some(owner) = owner_fqcn {
                        emit_method(&name, owner, arity, range_of(child), rel, nodes, edges, defs);
                    } else {
                        emit_function(&name, &name, arity, range_of(child), rel, file_node_id, nodes, edges, defs);
                    }
                }
            }
            _ => {}
        }
    }
}
