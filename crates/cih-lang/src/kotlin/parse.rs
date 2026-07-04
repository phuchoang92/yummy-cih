use anyhow::{Context, Result};
use cih_core::{
    constructor_id, field_id, file_id, function_id, method_id, type_id,
    Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit, Range, RawImport, SymbolDef,
};
use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_kotlin};
use tree_sitter::Node as TsNode;

fn range_of(node: TsNode<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start_line: start.row as u32 + 1,
        start_col: start.column as u32,
        end_line: end.row as u32 + 1,
        end_col: end.column as u32,
    }
}

fn text(node: TsNode<'_>, src: &str) -> String {
    node.utf8_text(src.as_bytes())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn first_simple_identifier(node: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "simple_identifier" {
            return Some(text(child, src));
        }
    }
    None
}

fn first_type_identifier(node: TsNode<'_>, src: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_identifier" {
            return Some(text(child, src));
        }
    }
    None
}

/// True if any child (including unnamed keywords) has the given kind.
fn has_child_kind(node: TsNode<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return true;
        }
    }
    false
}

fn count_class_parameters(primary_ctor: TsNode<'_>) -> u16 {
    let mut count = 0u16;
    let mut cursor = primary_ctor.walk();
    for child in primary_ctor.named_children(&mut cursor) {
        if child.kind() == "class_parameter" {
            count = count.saturating_add(1);
        }
    }
    count
}

fn count_parameters(fvp: TsNode<'_>) -> u16 {
    let mut count = 0u16;
    let mut cursor = fvp.walk();
    for child in fvp.named_children(&mut cursor) {
        if child.kind() == "parameter" {
            count = count.saturating_add(1);
        }
    }
    count
}

fn find_named_child<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

fn module_path(rel: &str) -> String {
    let stripped = rel
        .strip_suffix(".kts")
        .or_else(|| rel.strip_suffix(".kt"))
        .unwrap_or(rel);
    stripped.replace(['/', '\\'], ".")
}

#[derive(Default)]
struct Builder {
    rel: String,
    module: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    defs: Vec<SymbolDef>,
    imports: Vec<RawImport>,
}

impl Builder {
    fn contains_edge(&mut self, src: &NodeId, dst: &NodeId) {
        self.edges.push(Edge::new(
            src.clone(),
            dst.clone(),
            EdgeKind::Contains,
            1.0,
            "structure".into(),
        ));
    }

    fn has_method_edge(&mut self, owner: &NodeId, method: &NodeId) {
        self.edges.push(Edge::new(
            owner.clone(),
            method.clone(),
            EdgeKind::HasMethod,
            1.0,
            "member".into(),
        ));
    }

    fn has_field_edge(&mut self, owner: &NodeId, field: &NodeId) {
        self.edges.push(Edge::new(
            owner.clone(),
            field.clone(),
            EdgeKind::HasField,
            1.0,
            "member".into(),
        ));
    }
}

pub fn parse_kotlin_file(rel: &str, src: &str) -> Result<ParsedUnit> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_updated::language())
        .with_context(|| "failed to load Kotlin grammar")?;

    let tree = parser
        .parse(src, None)
        .with_context(|| format!("failed to parse {rel}"))?;

    let root = tree.root_node();

    // Extract package name to build FQCNs
    let package: Option<String> = {
        let mut cursor = root.walk();
        let mut found = None;
        for child in root.named_children(&mut cursor) {
            if child.kind() == "package_header" {
                let mut ic = child.walk();
                for c in child.named_children(&mut ic) {
                    if c.kind() == "identifier" {
                        found = Some(text(c, src));
                        break;
                    }
                }
                break;
            }
        }
        found
    };

    let module = package.clone().unwrap_or_else(|| module_path(rel));

    let mut builder = Builder {
        rel: rel.to_string(),
        module: module.clone(),
        ..Builder::default()
    };

    // Extract imports
    {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "import_list" {
                collect_imports(child, src, &mut builder);
            }
        }
    }

    // Walk top-level declarations
    let file_id_val = file_id(rel);
    {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            match child.kind() {
                "class_declaration" => {
                    emit_class_decl(child, src, &mut builder, &file_id_val, None);
                }
                "object_declaration" | "companion_object" => {
                    emit_object_decl(child, src, &mut builder, &file_id_val, None);
                }
                "function_declaration" => {
                    emit_function_decl(child, src, &mut builder, &file_id_val, None);
                }
                _ => {}
            }
        }
    }

    let import_bindings = builder.imports.iter().map(|imp| {
        use cih_core::{ImportBinding, ImportBindingKind};
        ImportBinding {
            module: imp.raw.clone(),
            imported: None,
            local: None,
            kind: if imp.is_wildcard {
                ImportBindingKind::Wildcard
            } else {
                ImportBindingKind::Named
            },
            range: imp.range,
        }
    }).collect();

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        import_bindings,
        parsed_file: ParsedFile {
            file: rel.to_string(),
            language: String::new(), // set by parse driver
            package,
            defs: builder.defs,
            imports: builder.imports,
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
            sql_constants: vec![],
            sql_execution_sites: vec![],
            string_constants: vec![],
        },
    })
}

fn collect_imports(import_list: TsNode<'_>, src: &str, builder: &mut Builder) {
    let mut cursor = import_list.walk();
    for child in import_list.named_children(&mut cursor) {
        if child.kind() != "import_header" {
            continue;
        }
        let range = range_of(child);
        let is_wildcard = has_child_kind(child, "wildcard_import");
        let mut raw_path = String::new();
        let mut ic = child.walk();
        for ic_child in child.named_children(&mut ic) {
            if ic_child.kind() == "identifier" {
                raw_path = text(ic_child, src);
                break;
            }
        }
        if raw_path.is_empty() {
            continue;
        }
        let raw = if is_wildcard {
            format!("{raw_path}.*")
        } else {
            raw_path
        };
        builder.imports.push(RawImport {
            raw,
            is_static: false,
            is_wildcard,
            range,
        });
    }
}

fn emit_class_decl(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    parent_id: &NodeId,
    outer_fqcn: Option<&str>,
) {
    let name = match first_type_identifier(node, src) {
        Some(n) => n,
        None => return,
    };
    let fqcn = match outer_fqcn {
        Some(outer) => format!("{outer}.{name}"),
        None => format!("{}.{name}", builder.module),
    };

    let is_interface = has_child_kind(node, "interface");
    let is_enum = find_named_child(node, "enum_class_body").is_some();
    let kind = if is_interface {
        NodeKind::Interface
    } else if is_enum {
        NodeKind::Enum
    } else {
        NodeKind::Class
    };

    let type_node_id = type_id(kind, &fqcn);
    let range = range_of(node);

    builder.nodes.push(Node {
        id: type_node_id.clone(),
        kind,
        name: name.clone(),
        qualified_name: Some(fqcn.clone()),
        file: builder.rel.clone(),
        range,
        props: None,
    });
    builder.contains_edge(parent_id, &type_node_id);
    builder.defs.push(SymbolDef {
        id: type_node_id.clone(),
        kind,
        fqcn: fqcn.clone(),
        name: name.clone(),
        owner: if outer_fqcn.is_some() {
            Some(parent_id.clone())
        } else {
            None
        },
        range,
        modifiers: vec![],
        param_types: vec![],
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    });

    // Primary constructor
    if let Some(ctor_node) = find_named_child(node, "primary_constructor") {
        let arity = count_class_parameters(ctor_node);
        let ctor_id = constructor_id(&fqcn, arity);
        let ctor_range = range_of(ctor_node);
        builder.nodes.push(Node {
            id: ctor_id.clone(),
            kind: NodeKind::Constructor,
            name: "<init>".into(),
            qualified_name: Some(format!("{fqcn}#<init>/{arity}")),
            file: builder.rel.clone(),
            range: ctor_range,
            props: None,
        });
        builder.contains_edge(&type_node_id, &ctor_id);
        builder.defs.push(SymbolDef {
            id: ctor_id,
            kind: NodeKind::Constructor,
            fqcn: fqcn.clone(),
            name: "<init>".into(),
            owner: Some(type_node_id.clone()),
            range: ctor_range,
            modifiers: vec![],
            param_types: vec![],
            return_type: None,
            declared_type: None,
            framework_role: None,
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
        });
    }

    // Class body
    let body_node = find_named_child(node, "class_body")
        .or_else(|| find_named_child(node, "enum_class_body"));
    if let Some(body) = body_node {
        walk_class_body(body, src, builder, &type_node_id, &fqcn);
    }
}

fn emit_object_decl(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    parent_id: &NodeId,
    outer_fqcn: Option<&str>,
) {
    let name = first_type_identifier(node, src).unwrap_or_else(|| "Companion".to_string());
    let fqcn = match outer_fqcn {
        Some(outer) => format!("{outer}.{name}"),
        None => format!("{}.{name}", builder.module),
    };

    let type_node_id = type_id(NodeKind::Class, &fqcn);
    let range = range_of(node);

    builder.nodes.push(Node {
        id: type_node_id.clone(),
        kind: NodeKind::Class,
        name: name.clone(),
        qualified_name: Some(fqcn.clone()),
        file: builder.rel.clone(),
        range,
        props: None,
    });
    builder.contains_edge(parent_id, &type_node_id);
    builder.defs.push(SymbolDef {
        id: type_node_id.clone(),
        kind: NodeKind::Class,
        fqcn: fqcn.clone(),
        name: name.clone(),
        owner: if outer_fqcn.is_some() {
            Some(parent_id.clone())
        } else {
            None
        },
        range,
        modifiers: vec![],
        param_types: vec![],
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    });

    if let Some(body) = find_named_child(node, "class_body") {
        walk_class_body(body, src, builder, &type_node_id, &fqcn);
    }
}

fn walk_class_body(
    body: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    type_node_id: &NodeId,
    fqcn: &str,
) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                emit_function_decl(child, src, builder, type_node_id, Some(fqcn));
            }
            "property_declaration" => {
                emit_property_decl(child, src, builder, type_node_id, fqcn);
            }
            "secondary_constructor" => {
                emit_secondary_constructor(child, src, builder, type_node_id, fqcn);
            }
            "class_declaration" => {
                emit_class_decl(child, src, builder, type_node_id, Some(fqcn));
            }
            "object_declaration" | "companion_object" => {
                emit_object_decl(child, src, builder, type_node_id, Some(fqcn));
            }
            _ => {}
        }
    }
}

fn emit_function_decl(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    parent_id: &NodeId,
    in_fqcn: Option<&str>,
) {
    let name = match first_simple_identifier(node, src) {
        Some(n) => n,
        None => return,
    };
    let arity = find_named_child(node, "function_value_parameters")
        .map(count_parameters)
        .unwrap_or(0);

    let body_fp = find_named_child(node, "function_body")
        .and_then(|b| compute_body_fingerprint(b, "kotlin", normalize_leaf_token_kotlin));

    let range = range_of(node);

    match in_fqcn {
        Some(fqcn) => {
            let node_id = method_id(fqcn, &name, arity);
            builder.nodes.push(Node {
                id: node_id.clone(),
                kind: NodeKind::Method,
                name: name.clone(),
                qualified_name: Some(format!("{fqcn}#{name}/{arity}")),
                file: builder.rel.clone(),
                range,
                props: None,
            });
            builder.has_method_edge(parent_id, &node_id);
            builder.defs.push(SymbolDef {
                id: node_id,
                kind: NodeKind::Method,
                fqcn: fqcn.to_string(),
                name,
                owner: Some(parent_id.clone()),
                range,
                modifiers: vec![],
                param_types: vec![],
                return_type: None,
                declared_type: None,
                framework_role: None,
                complexity: None,
                body_fingerprint: body_fp,
                lang_meta: None,
            });
        }
        None => {
            let node_id = function_id(&builder.module, &name, arity);
            builder.nodes.push(Node {
                id: node_id.clone(),
                kind: NodeKind::Function,
                name: name.clone(),
                qualified_name: Some(format!("{}#{name}/{arity}", builder.module)),
                file: builder.rel.clone(),
                range,
                props: None,
            });
            builder.contains_edge(parent_id, &node_id);
            builder.defs.push(SymbolDef {
                id: node_id,
                kind: NodeKind::Function,
                fqcn: builder.module.clone(),
                name,
                owner: None,
                range,
                modifiers: vec![],
                param_types: vec![],
                return_type: None,
                declared_type: None,
                framework_role: None,
                complexity: None,
                body_fingerprint: body_fp,
                lang_meta: None,
            });
        }
    }
}

fn emit_property_decl(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    parent_id: &NodeId,
    in_fqcn: &str,
) {
    let name = {
        let mut cursor = node.walk();
        let mut found = None;
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declaration" {
                found = first_simple_identifier(child, src);
                break;
            }
            if child.kind() == "simple_identifier" {
                found = Some(text(child, src));
                break;
            }
        }
        match found {
            Some(n) => n,
            None => return,
        }
    };

    let field_node_id = field_id(in_fqcn, &name);
    let range = range_of(node);

    builder.nodes.push(Node {
        id: field_node_id.clone(),
        kind: NodeKind::Field,
        name: name.clone(),
        qualified_name: Some(format!("{in_fqcn}#{name}")),
        file: builder.rel.clone(),
        range,
        props: None,
    });
    builder.has_field_edge(parent_id, &field_node_id);
    builder.defs.push(SymbolDef {
        id: field_node_id,
        kind: NodeKind::Field,
        fqcn: in_fqcn.to_string(),
        name,
        owner: Some(parent_id.clone()),
        range,
        modifiers: vec![],
        param_types: vec![],
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    });
}

fn emit_secondary_constructor(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    parent_id: &NodeId,
    in_fqcn: &str,
) {
    let arity = find_named_child(node, "function_value_parameters")
        .map(count_parameters)
        .unwrap_or(0);
    let ctor_id = constructor_id(in_fqcn, arity);
    let range = range_of(node);

    builder.nodes.push(Node {
        id: ctor_id.clone(),
        kind: NodeKind::Constructor,
        name: "<init>".into(),
        qualified_name: Some(format!("{in_fqcn}#<init>/{arity}")),
        file: builder.rel.clone(),
        range,
        props: None,
    });
    builder.contains_edge(parent_id, &ctor_id);
    builder.defs.push(SymbolDef {
        id: ctor_id,
        kind: NodeKind::Constructor,
        fqcn: in_fqcn.to_string(),
        name: "<init>".into(),
        owner: Some(parent_id.clone()),
        range,
        modifiers: vec![],
        param_types: vec![],
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
    lang_meta: None,
    });

    let _ = src;
}
