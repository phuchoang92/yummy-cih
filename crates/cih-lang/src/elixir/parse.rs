use cih_core::{file_id, ParsedUnit, RawImport};
use tree_sitter::Node as TsNode;
use crate::generic_parse::{emit_function, emit_type, range_of, text, build_unit};
use cih_core::{Edge, Node, NodeKind, SymbolDef, ReferenceSite};

pub fn parse_elixir_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser.parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut imports: Vec<RawImport> = Vec::new();
    let sites: Vec<ReferenceSite> = Vec::new();

    walk(root, src, rel, &file_node_id, None,
         &mut nodes, &mut edges, &mut defs, &mut imports);

    Ok(build_unit(rel, "elixir", None, nodes, edges, defs, imports, sites))
}

#[allow(clippy::too_many_arguments)] // recursive tree-walker signature
fn walk(
    parent: TsNode<'_>, src: &str, rel: &str,
    file_node_id: &cih_core::NodeId, module_name: Option<&str>,
    nodes: &mut Vec<Node>, edges: &mut Vec<Edge>, defs: &mut Vec<SymbolDef>,
    imports: &mut Vec<RawImport>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        // Elixir: `defmodule ModuleName do ... end`
        if child.kind() == "call" {
            let func = child.child_by_field_name("target");
            let func_name = func.map(|n| text(n, src)).unwrap_or("");
            match func_name {
                "defmodule" => {
                    if let Some(args) = child.child_by_field_name("arguments") {
                        let mut ac = args.walk();
                        for arg in args.named_children(&mut ac) {
                            let name = text(arg, src).to_string();
                            if !name.is_empty() && name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                                emit_type(NodeKind::Class, &name, &name, range_of(child), rel, file_node_id, nodes, edges, defs);
                                walk(child, src, rel, file_node_id, Some(&name), nodes, edges, defs, imports);
                            }
                        }
                    }
                }
                "def" | "defp" => {
                    if let Some(args) = child.child_by_field_name("arguments") {
                        // Find the first named child index without holding the cursor alive
                        let first_idx = (0..args.child_count())
                            .find(|&i| args.child(i).is_some_and(|c| c.is_named()));
                        let name: String = first_idx
                            .and_then(|i| args.child(i))
                            .map(|first| {
                                if first.kind() == "call" {
                                    first.child_by_field_name("target")
                                        .map(|n| text(n, src).to_string())
                                        .unwrap_or_default()
                                } else {
                                    text(first, src).to_string()
                                }
                            })
                            .unwrap_or_default();
                        if !name.is_empty() {
                            let fqcn = match module_name {
                                Some(m) => format!("{m}.{name}"),
                                None => name.clone(),
                            };
                            emit_function(&name, &fqcn, 0, range_of(child), rel, file_node_id, nodes, edges, defs);
                        }
                    }
                }
                "import" | "alias" | "use" | "require" => {
                    if let Some(args) = child.child_by_field_name("arguments") {
                        let raw = text(args, src).to_string();
                        if !raw.is_empty() {
                            imports.push(RawImport { raw, is_static: false, is_wildcard: false, alias: None, range: range_of(child) });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
