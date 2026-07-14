use std::collections::HashMap;

use cih_core::{
    file_id, function_id, type_id, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit,
    Range, RawImport, RefKind, ReferenceSite, SymbolDef,
};
use tree_sitter::Node as TsNode;

use super::framework;

pub(super) fn range_of(node: TsNode<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start_line: start.row as u32 + 1,
        start_col: start.column as u32,
        end_line: end.row as u32 + 1,
        end_col: end.column as u32,
    }
}

pub(super) fn text<'a>(node: TsNode<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("").trim()
}

pub(super) fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') || s.starts_with('`') && s.ends_with('`'))
        && s.len() >= 2
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn param_count(params_node: TsNode<'_>) -> u16 {
    let mut count = 0u16;
    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        if child.kind() == "parameter_declaration" || child.kind() == "variadic_parameter_declaration" {
            count += 1;
        }
    }
    count
}

fn extract_package(root: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_clause" {
            let mut ic = child.walk();
            for c in child.named_children(&mut ic) {
                if c.kind() == "package_identifier" {
                    return Some(text(c, src).to_string());
                }
            }
        }
    }
    None
}

/// Extract receiver type name from a method_declaration receiver list,
/// e.g. `(s *UserService)` → `"UserService"`
fn extract_receiver_type(receiver_node: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = receiver_node.walk();
    for child in receiver_node.named_children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            let mut ic = child.walk();
            for c in child.named_children(&mut ic) {
                match c.kind() {
                    "pointer_type" => {
                        // *TypeName → get the type_identifier inside
                        let mut pc = c.walk();
                        for p in c.named_children(&mut pc) {
                            if p.kind() == "type_identifier" {
                                return Some(text(p, src).to_string());
                            }
                        }
                    }
                    "type_identifier" => {
                        return Some(text(c, src).to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

pub fn parse_go_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();

    let pkg = extract_package(root, src).unwrap_or_else(|| "main".to_string());
    let file_node_id = file_id(rel);

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut defs: Vec<SymbolDef> = Vec::new();
    let mut imports: Vec<RawImport> = Vec::new();
    let mut reference_sites: Vec<ReferenceSite> = Vec::new();
    let mut contract_sites: Vec<cih_core::ContractSite> = Vec::new();
    // Function/method bodies to re-walk for framework contracts, plus the
    // same-file `name → id` map used to resolve plain-identifier handlers.
    let mut callable_bodies: Vec<(TsNode<'_>, NodeId)> = Vec::new();
    let mut file_fn_ids: HashMap<String, NodeId> = HashMap::new();

    // Walk top-level declarations
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "import_declaration" => {
                collect_imports(child, src, &mut imports);
            }
            "function_declaration" => {
                if let Some((def, node, edge)) =
                    extract_function(child, src, rel, &pkg, &file_node_id)
                {
                    collect_calls(child, src, &def.fqcn, &def.id, &mut reference_sites);
                    file_fn_ids.insert(def.name.clone(), def.id.clone());
                    callable_bodies.push((child, def.id.clone()));
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "method_declaration" => {
                if let Some((def, node, edge)) =
                    extract_method(child, src, rel, &pkg, &file_node_id)
                {
                    collect_calls(child, src, &def.fqcn, &def.id, &mut reference_sites);
                    callable_bodies.push((child, def.id.clone()));
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "type_declaration" => {
                collect_type_decls(child, src, rel, &pkg, &file_node_id, &mut defs, &mut nodes, &mut edges);
            }
            _ => {}
        }
    }

    // Framework pass — import-gated; most files skip it entirely.
    let framework_ctx = framework::GoFrameworkCtx::from_imports(&imports);
    if framework_ctx.any() {
        for (body, callable_id) in &callable_bodies {
            framework::collect_contracts(
                *body,
                src,
                &framework_ctx,
                callable_id,
                &file_fn_ids,
                rel,
                &mut nodes,
                &mut edges,
                &mut contract_sites,
            );
        }
    }

    let parsed_file = ParsedFile {
        file: rel.to_string(),
        language: "go".to_string(),
        package: Some(pkg),
        defs,
        imports,
        reference_sites,
        contract_sites,
        ..Default::default()
    };

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes,
        edges,
        parsed_file,
    })
}

fn collect_imports(import_decl: TsNode<'_>, src: &str, imports: &mut Vec<RawImport>) {
    let mut cursor = import_decl.walk();
    for child in import_decl.named_children(&mut cursor) {
        match child.kind() {
            "import_spec" => {
                if let Some(path_node) = child.child_by_field_name("path") {
                    let raw = unquote(text(path_node, src));
                    imports.push(RawImport {
                        raw,
                        is_static: false,
                        is_wildcard: false,
                        alias: None,
                        range: range_of(child),
                    });
                }
            }
            "import_spec_list" => {
                let mut ic = child.walk();
                for spec in child.named_children(&mut ic) {
                    if spec.kind() == "import_spec" {
                        if let Some(path_node) = spec.child_by_field_name("path") {
                            let raw = unquote(text(path_node, src));
                            imports.push(RawImport {
                                raw,
                                is_static: false,
                                is_wildcard: false,
                                alias: None,
                                range: range_of(spec),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn extract_function(
    node: TsNode<'_>,
    src: &str,
    rel: &str,
    pkg: &str,
    file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let name_node = node.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let arity = node
        .child_by_field_name("parameters")
        .map(|p| param_count(p))
        .unwrap_or(0);
    let fqcn = format!("{pkg}.{name}");
    let id = function_id(&fqcn, &name, arity);
    let range = range_of(node);

    let def = SymbolDef {
        id: id.clone(),
        kind: NodeKind::Function,
        fqcn: fqcn.clone(),
        name: name.clone(),
        owner: None,
        range,
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(),
        kind: NodeKind::Function,
        name: name.clone(),
        qualified_name: Some(fqcn),
        file: rel.to_string(),
        range,
        props: None,
    };
    let edge = Edge {
        src: file_id.clone(),
        dst: id,
        kind: EdgeKind::Contains,
        confidence: 1.0,
        reason: "structure".into(),
        props: None,
    };
    Some((def, graph_node, edge))
}

fn extract_method(
    node: TsNode<'_>,
    src: &str,
    rel: &str,
    pkg: &str,
    _file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let receiver_node = node.child_by_field_name("receiver")?;
    let receiver_type = extract_receiver_type(receiver_node, src)?;
    let name_node = node.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let arity = node
        .child_by_field_name("parameters")
        .map(|p| param_count(p))
        .unwrap_or(0);
    let owner_fqcn = format!("{pkg}.{receiver_type}");
    let fqcn = format!("{owner_fqcn}.{name}");
    let owner_id = type_id(NodeKind::Class, &owner_fqcn);
    let id = cih_core::method_id(&owner_fqcn, &name, arity);
    let range = range_of(node);

    let def = SymbolDef {
        id: id.clone(),
        kind: NodeKind::Method,
        fqcn: owner_fqcn,
        name: name.clone(),
        owner: Some(owner_id.clone()),
        range,
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(),
        kind: NodeKind::Method,
        name: name.clone(),
        qualified_name: Some(fqcn),
        file: rel.to_string(),
        range,
        props: None,
    };
    let edge = Edge {
        src: owner_id,
        dst: id,
        kind: EdgeKind::HasMethod,
        confidence: 1.0,
        reason: "structure".into(),
        props: None,
    };
    Some((def, graph_node, edge))
}

#[allow(clippy::too_many_arguments)] // recursive tree-walker signature
fn collect_type_decls(
    type_decl: TsNode<'_>,
    src: &str,
    rel: &str,
    pkg: &str,
    file_id: &NodeId,
    defs: &mut Vec<SymbolDef>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    let mut cursor = type_decl.walk();
    for child in type_decl.named_children(&mut cursor) {
        if child.kind() == "type_spec" {
            if let Some((def, node, edge)) = extract_type_spec(child, src, rel, pkg, file_id) {
                defs.push(def);
                nodes.push(node);
                edges.push(edge);
            }
        }
    }
}

fn extract_type_spec(
    spec: TsNode<'_>,
    src: &str,
    rel: &str,
    pkg: &str,
    file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let name_node = spec.child_by_field_name("name")?;
    let name = text(name_node, src).to_string();
    let type_node = spec.child_by_field_name("type")?;
    let kind = match type_node.kind() {
        "struct_type" => NodeKind::Class,
        "interface_type" => NodeKind::Interface,
        _ => NodeKind::Class,
    };
    let fqcn = format!("{pkg}.{name}");
    let id = type_id(kind, &fqcn);
    let range = range_of(spec);

    let def = SymbolDef {
        id: id.clone(),
        kind,
        fqcn: fqcn.clone(),
        name: name.clone(),
        owner: None,
        range,
        modifiers: Vec::new(),
        param_types: Vec::new(),
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(),
        kind,
        name: name.clone(),
        qualified_name: Some(fqcn),
        file: rel.to_string(),
        range,
        props: None,
    };
    let edge = Edge {
        src: file_id.clone(),
        dst: id,
        kind: EdgeKind::Contains,
        confidence: 1.0,
        reason: "structure".into(),
        props: None,
    };
    Some((def, graph_node, edge))
}

/// Walk the body of a function/method and collect call expressions.
fn collect_calls(
    body_root: TsNode<'_>,
    src: &str,
    in_fqcn: &str,
    in_callable: &NodeId,
    sites: &mut Vec<ReferenceSite>,
) {
    let mut stack = vec![body_root];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            if let Some(func_node) = node.child_by_field_name("function") {
                let (receiver, name) = match func_node.kind() {
                    "selector_expression" => {
                        let operand = func_node.child_by_field_name("operand");
                        let field = func_node.child_by_field_name("field");
                        (
                            operand.map(|n| text(n, src).to_string()),
                            field.map(|n| text(n, src).to_string()).unwrap_or_default(),
                        )
                    }
                    _ => (None, text(func_node, src).to_string()),
                };
                if !name.is_empty() {
                    let arity = node
                        .child_by_field_name("arguments")
                        .map(|a| {
                            a.named_child_count().saturating_sub(0) as u16
                        })
                        .unwrap_or(0);
                    sites.push(ReferenceSite {
                        name,
                        receiver,
                        kind: RefKind::Call,
                        arity: Some(arity),
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
