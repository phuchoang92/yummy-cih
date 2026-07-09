use cih_core::{file_id, ParsedUnit, RawImport};
use crate::generic_parse::{emit_function, range_of, text, build_unit};
use cih_core::{Edge, Node, SymbolDef, ReferenceSite};

pub fn parse_bash_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser.parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let imports: Vec<RawImport> = Vec::new();
    let sites: Vec<ReferenceSite> = Vec::new();

    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "function_definition" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = text(name_node, src).to_string();
                emit_function(&name, &name, 0, range_of(child), rel, &file_node_id, &mut nodes, &mut edges, &mut defs);
            }
        }
    }

    Ok(build_unit(rel, "bash", None, nodes, edges, defs, imports, sites))
}
