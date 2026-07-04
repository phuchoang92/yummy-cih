use cih_core::{file_id, ParsedUnit, RawImport};
use tree_sitter::Node as TsNode;
use crate::generic_parse::{emit_function, emit_method, emit_type, range_of, text, build_unit};
use cih_core::{Edge, Node, NodeKind, SymbolDef, ReferenceSite};

fn package_of(root: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_clause" {
            if let Some(name) = child.child_by_field_name("name") {
                return Some(text(name, src).to_string());
            }
        }
    }
    None
}

pub fn parse_scala_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser.parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let pkg = package_of(root, src).unwrap_or_default();
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut imports: Vec<RawImport> = Vec::new();
    let mut sites: Vec<ReferenceSite> = Vec::new();

    walk(root, src, rel, &pkg, &file_node_id, None,
         &mut nodes, &mut edges, &mut defs, &mut imports, &mut sites);

    Ok(build_unit(rel, "scala", if pkg.is_empty() { None } else { Some(pkg) },
                  nodes, edges, defs, imports, sites))
}

fn walk(
    parent: TsNode<'_>, src: &str, rel: &str, pkg: &str,
    file_node_id: &cih_core::NodeId, owner_fqcn: Option<&str>,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
    imports: &mut Vec<RawImport>, sites: &mut Vec<ReferenceSite>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "import_declaration" => {
                if let Some(expr) = child.child_by_field_name("path") {
                    let raw = text(expr, src).to_string();
                    let is_wildcard = raw.ends_with("._") || raw.ends_with(".*");
                    imports.push(RawImport { raw, is_static: false, is_wildcard, range: range_of(child) });
                }
            }
            "class_definition" | "object_definition" | "case_class_definition" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let fqcn = if pkg.is_empty() { name.clone() } else { format!("{pkg}.{name}") };
                    emit_type(NodeKind::Class, &name, &fqcn, range_of(child), rel, file_node_id, nodes, edges, defs);
                    walk(child, src, rel, pkg, file_node_id, Some(&fqcn),
                         nodes, edges, defs, imports, sites);
                }
            }
            "trait_definition" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let fqcn = if pkg.is_empty() { name.clone() } else { format!("{pkg}.{name}") };
                    emit_type(NodeKind::Interface, &name, &fqcn, range_of(child), rel, file_node_id, nodes, edges, defs);
                }
            }
            "function_definition" | "function_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = text(name_node, src).to_string();
                    let arity = child.child_by_field_name("parameters")
                        .map(|p| p.named_child_count() as u16).unwrap_or(0);
                    if let Some(owner) = owner_fqcn {
                        emit_method(&name, owner, arity, range_of(child), rel, nodes, edges, defs);
                    } else {
                        let fqcn = if pkg.is_empty() { name.clone() } else { format!("{pkg}.{name}") };
                        emit_function(&name, &fqcn, arity, range_of(child), rel, file_node_id, nodes, edges, defs);
                    }
                }
            }
            _ => {}
        }
    }
}
