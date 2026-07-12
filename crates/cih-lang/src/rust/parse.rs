use cih_core::{
    file_id, function_id, type_id, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit,
    Range, RawImport, RefKind, ReferenceSite, SymbolDef,
};
use tree_sitter::Node as TsNode;

fn range_of(node: TsNode<'_>) -> Range {
    let s = node.start_position();
    let e = node.end_position();
    Range { start_line: s.row as u32 + 1, start_col: s.column as u32,
            end_line: e.row as u32 + 1, end_col: e.column as u32 }
}

fn text<'a>(node: TsNode<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("").trim()
}

/// Best-effort module path from the file path: `src/users/service.rs` → `users::service`
fn module_path(rel: &str) -> String {
    let stripped = rel.strip_suffix(".rs").unwrap_or(rel);
    let stripped = stripped.strip_prefix("src/").unwrap_or(stripped);
    let stripped = if stripped == "lib" || stripped == "main" { "" } else { stripped };
    stripped.replace('/', "::")
}

pub fn parse_rust_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser.parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let module = module_path(rel);
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut imports: Vec<RawImport> = Vec::new();
    let mut reference_sites: Vec<ReferenceSite> = Vec::new();

    walk_items(root, src, rel, &module, &file_node_id, None,
               &mut defs, &mut nodes, &mut edges, &mut imports, &mut reference_sites);

    let parsed_file = ParsedFile {
        file: rel.to_string(),
        language: "rust".to_string(),
        package: if module.is_empty() { None } else { Some(module) },
        defs,
        imports,
        reference_sites,
        ..Default::default()
    };
    Ok(ParsedUnit { rel: rel.to_string(), nodes, edges, parsed_file, import_bindings: Vec::new() })
}

#[allow(clippy::too_many_arguments)] // recursive tree-walker signature
fn walk_items(
    parent: TsNode<'_>, src: &str, rel: &str, module: &str,
    file_id: &NodeId, owner_fqcn: Option<&str>,
    defs: &mut Vec<SymbolDef>, nodes: &mut Vec<Node>, edges: &mut Vec<Edge>,
    imports: &mut Vec<RawImport>, sites: &mut Vec<ReferenceSite>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "use_declaration" => collect_use(child, src, imports),
            "function_item" => {
                if let Some((def, node, edge)) =
                    extract_fn(child, src, rel, module, file_id, owner_fqcn)
                {
                    collect_calls(child, src, &def.fqcn, &def.id, sites);
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "struct_item" | "enum_item" | "type_item" => {
                if let Some((def, node, edge)) =
                    extract_type(child, src, rel, module, file_id)
                {
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "trait_item" => {
                if let Some((def, node, edge)) =
                    extract_trait(child, src, rel, module, file_id)
                {
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "impl_item" => {
                // Walk methods inside impl blocks
                let self_type = child.child_by_field_name("type")
                    .map(|t| text(t, src).to_string());
                let owner = self_type.as_deref().map(|t| {
                    if module.is_empty() { t.to_string() } else { format!("{module}::{t}") }
                });
                walk_items(child, src, rel, module, file_id, owner.as_deref(),
                           defs, nodes, edges, imports, sites);
            }
            "mod_item" => {
                // Inline modules
                walk_items(child, src, rel, module, file_id, owner_fqcn,
                           defs, nodes, edges, imports, sites);
            }
            _ => {}
        }
    }
}

fn extract_fn(
    node: TsNode<'_>, src: &str, rel: &str, module: &str,
    file_id: &NodeId, owner_fqcn: Option<&str>,
) -> Option<(SymbolDef, Node, Edge)> {
    let name_node = node.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let arity = node.child_by_field_name("parameters")
        .map(|p| p.named_child_count() as u16).unwrap_or(0);

    let (fqcn, kind, owner_id) = if let Some(owner) = owner_fqcn {
        let fqcn = format!("{owner}::{name}");
        let owner_id = type_id(NodeKind::Class, owner);
        (fqcn, NodeKind::Method, Some(owner_id))
    } else {
        let fqcn = if module.is_empty() { name.clone() } else { format!("{module}::{name}") };
        (fqcn, NodeKind::Function, None)
    };

    let id = if kind == NodeKind::Method {
        cih_core::method_id(owner_fqcn.unwrap_or(""), &name, arity)
    } else {
        function_id(&fqcn, &name, arity)
    };
    let range = range_of(node);

    let def = SymbolDef {
        id: id.clone(), kind, fqcn: owner_fqcn.unwrap_or(&fqcn).to_string(),
        name: name.clone(), owner: owner_id.clone(),
        range, modifiers: Vec::new(), param_types: Vec::new(),
        return_type: None, declared_type: None, framework_role: None,
        complexity: None, body_fingerprint: None,
    lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(), kind, name: name.clone(),
        qualified_name: Some(fqcn), file: rel.to_string(), range, props: None,
    };
    let src_id = owner_id.unwrap_or_else(|| file_id.clone());
    let edge_kind = if kind == NodeKind::Method { EdgeKind::HasMethod } else { EdgeKind::Contains };
    let edge = Edge { src: src_id, dst: id, kind: edge_kind, confidence: 1.0,
                      reason: "structure".into(), props: None };
    Some((def, graph_node, edge))
}

fn extract_type(
    node: TsNode<'_>, src: &str, rel: &str, module: &str, file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let name_node = node.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let kind = if node.kind() == "enum_item" { NodeKind::Enum } else { NodeKind::Class };
    let fqcn = if module.is_empty() { name.clone() } else { format!("{module}::{name}") };
    let id = type_id(kind, &fqcn);
    let range = range_of(node);
    let def = SymbolDef {
        id: id.clone(), kind, fqcn: fqcn.clone(), name: name.clone(),
        owner: None, range, modifiers: Vec::new(), param_types: Vec::new(),
        return_type: None, declared_type: None, framework_role: None,
        complexity: None, body_fingerprint: None,
    lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(), kind, name: name.clone(),
        qualified_name: Some(fqcn), file: rel.to_string(), range, props: None,
    };
    let edge = Edge { src: file_id.clone(), dst: id, kind: EdgeKind::Contains,
                      confidence: 1.0, reason: "structure".into(), props: None };
    Some((def, graph_node, edge))
}

fn extract_trait(
    node: TsNode<'_>, src: &str, rel: &str, module: &str, file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let name_node = node.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let fqcn = if module.is_empty() { name.clone() } else { format!("{module}::{name}") };
    let id = type_id(NodeKind::Interface, &fqcn);
    let range = range_of(node);
    let def = SymbolDef {
        id: id.clone(), kind: NodeKind::Interface, fqcn: fqcn.clone(),
        name: name.clone(), owner: None, range, modifiers: Vec::new(),
        param_types: Vec::new(), return_type: None, declared_type: None,
        framework_role: None, complexity: None, body_fingerprint: None,
    lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(), kind: NodeKind::Interface, name: name.clone(),
        qualified_name: Some(fqcn), file: rel.to_string(), range, props: None,
    };
    let edge = Edge { src: file_id.clone(), dst: id, kind: EdgeKind::Contains,
                      confidence: 1.0, reason: "structure".into(), props: None };
    Some((def, graph_node, edge))
}

fn collect_use(node: TsNode<'_>, src: &str, imports: &mut Vec<RawImport>) {
    // `use foo::bar::Baz;` → raw = "foo::bar::Baz"
    if let Some(arg) = node.child_by_field_name("argument") {
        let raw = text(arg, src).to_string();
        let is_wildcard = raw.ends_with("::*");
        imports.push(RawImport {
            raw: raw.trim_end_matches("::*").to_string(),
            is_static: false,
            is_wildcard,
            alias: None,
            range: range_of(node),
        });
    }
}

fn collect_calls(
    root: TsNode<'_>, src: &str, in_fqcn: &str, in_callable: &NodeId,
    sites: &mut Vec<ReferenceSite>,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let (receiver, name) = match func.kind() {
                    "field_expression" => {
                        let val = func.child_by_field_name("value");
                        let field = func.child_by_field_name("field");
                        (val.map(|n| text(n, src).to_string()),
                         field.map(|n| text(n, src).to_string()).unwrap_or_default())
                    }
                    _ => (None, text(func, src).to_string()),
                };
                if !name.is_empty() {
                    sites.push(ReferenceSite {
                        name, receiver, kind: RefKind::Call,
                        arity: Some(node.child_by_field_name("arguments")
                            .map(|a| a.named_child_count() as u16).unwrap_or(0)),
                        range: range_of(node),
                        in_fqcn: in_fqcn.to_string(),
                        in_callable: in_callable.clone(),
                        arg_texts: Vec::new(),
                    });
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}
