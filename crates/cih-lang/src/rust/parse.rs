use cih_core::{
    file_id, function_id, method_id, type_id, BindingKind, Edge, EdgeKind, Node, NodeId,
    NodeKind, ParsedFile, ParsedUnit, Range, RawImport, RefKind, ReferenceSite, SymbolDef,
    TypeBinding,
};
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

fn text<'a>(node: TsNode<'_>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("").trim()
}

/// Best-effort module path from the file path: `src/users/service.rs` →
/// `users::service`. `lib.rs` and `main.rs` represent the crate root.
fn module_path(rel: &str) -> String {
    let stripped = rel.strip_suffix(".rs").unwrap_or(rel);
    let stripped = stripped.strip_prefix("src/").unwrap_or(stripped);
    let stripped = if stripped == "lib" || stripped == "main" {
        ""
    } else {
        stripped
    };
    stripped.replace('/', "::")
}

#[derive(Clone, Debug)]
struct Owner {
    fqcn: String,
    kind: NodeKind,
}

pub fn parse_rust_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = super::make_parser();
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter failed to parse {rel}"))?;
    let root = tree.root_node();
    let module = module_path(rel);
    let file_node_id = file_id(rel);

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut defs = Vec::new();
    let mut imports = Vec::new();
    let mut reference_sites = Vec::new();
    let mut type_bindings = Vec::new();

    walk_items(
        root,
        src,
        rel,
        &module,
        &file_node_id,
        None,
        &mut defs,
        &mut nodes,
        &mut edges,
        &mut imports,
        &mut reference_sites,
        &mut type_bindings,
    );

    let parsed_file = ParsedFile {
        file: rel.to_string(),
        language: "rust".to_string(),
        package: (!module.is_empty()).then_some(module),
        defs,
        imports,
        reference_sites,
        type_bindings,
        ..Default::default()
    };
    let syntactic_callables =
        crate::generic_parse::count_callables(root, super::RUST_CALLABLE_KINDS);
    Ok(ParsedUnit {
        rel: rel.to_string(),
        syntactic_callables,
        nodes,
        edges,
        parsed_file,
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_items(
    parent: TsNode<'_>,
    src: &str,
    rel: &str,
    module: &str,
    file_id: &NodeId,
    owner: Option<&Owner>,
    defs: &mut Vec<SymbolDef>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    imports: &mut Vec<RawImport>,
    sites: &mut Vec<ReferenceSite>,
    bindings: &mut Vec<TypeBinding>,
) {
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        match child.kind() {
            "declaration_list" => walk_items(
                child, src, rel, module, file_id, owner, defs, nodes, edges, imports, sites,
                bindings,
            ),
            "use_declaration" => collect_use(child, src, imports),
            "function_item" | "function_signature_item" => {
                if let Some((def, node, edge, callable_scope)) =
                    extract_fn(child, src, rel, module, file_id, owner)
                {
                    if child.kind() == "function_item" {
                        collect_calls(child, src, &callable_scope, &def.id, sites);
                        collect_type_bindings(child, src, &callable_scope, bindings);
                    } else {
                        collect_parameter_bindings(child, src, &callable_scope, bindings);
                    }
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "struct_item" | "enum_item" | "type_item" => {
                if let Some((def, node, edge)) = extract_type(child, src, rel, module, file_id) {
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                }
            }
            "trait_item" => {
                if let Some((def, node, edge)) = extract_trait(child, src, rel, module, file_id) {
                    let trait_owner = Owner {
                        fqcn: def.fqcn.clone(),
                        kind: NodeKind::Interface,
                    };
                    let body = child.child_by_field_name("body");
                    defs.push(def);
                    nodes.push(node);
                    edges.push(edge);
                    if let Some(body) = body {
                        walk_items(
                            body,
                            src,
                            rel,
                            module,
                            file_id,
                            Some(&trait_owner),
                            defs,
                            nodes,
                            edges,
                            imports,
                            sites,
                            bindings,
                        );
                    }
                }
            }
            "impl_item" => {
                let Some(raw_type) = child.child_by_field_name("type").map(|node| text(node, src))
                else {
                    continue;
                };
                let owner_fqcn = qualify_type(module, raw_type);
                let parsed_owner_kind = defs
                    .iter()
                    .find(|definition| {
                        definition.fqcn == owner_fqcn
                            && matches!(
                                definition.kind,
                                NodeKind::Class | NodeKind::Enum | NodeKind::Interface
                            )
                    })
                    .map(|definition| definition.kind);
                let owner_kind = parsed_owner_kind.unwrap_or(NodeKind::Class);
                if parsed_owner_kind.is_none() {
                    let owner_id = type_id(owner_kind, &owner_fqcn);
                    if !nodes.iter().any(|node| node.id == owner_id) {
                        nodes.push(Node {
                            id: owner_id.clone(),
                            kind: owner_kind,
                            name: raw_type.rsplit("::").next().unwrap_or(raw_type).to_string(),
                            qualified_name: Some(owner_fqcn.clone()),
                            file: rel.to_string(),
                            range: range_of(child),
                            props: Some(serde_json::json!({
                                "source": "rust_impl",
                                "external_owner": true,
                            })),
                        });
                        edges.push(Edge {
                            src: file_id.clone(),
                            dst: owner_id,
                            kind: EdgeKind::Contains,
                            confidence: 1.0,
                            reason: "rust-impl-owner".into(),
                            props: None,
                        });
                    }
                }
                let impl_owner = Owner {
                    fqcn: owner_fqcn,
                    kind: owner_kind,
                };
                if let Some(body) = child.child_by_field_name("body") {
                    walk_items(
                        body,
                        src,
                        rel,
                        module,
                        file_id,
                        Some(&impl_owner),
                        defs,
                        nodes,
                        edges,
                        imports,
                        sites,
                        bindings,
                    );
                }
            }
            "mod_item" => {
                let Some(body) = child.child_by_field_name("body") else {
                    continue;
                };
                let Some(name) = child.child_by_field_name("name") else {
                    continue;
                };
                let nested_module = join_path(module, text(name, src));
                walk_items(
                    body,
                    src,
                    rel,
                    &nested_module,
                    file_id,
                    None,
                    defs,
                    nodes,
                    edges,
                    imports,
                    sites,
                    bindings,
                );
            }
            _ => {}
        }
    }
}

fn extract_fn(
    node: TsNode<'_>,
    src: &str,
    rel: &str,
    module: &str,
    file_id: &NodeId,
    owner: Option<&Owner>,
) -> Option<(SymbolDef, Node, Edge, String)> {
    let name = text(node.child_by_field_name("name")?, src).to_string();
    let params = node.child_by_field_name("parameters");
    let param_types = params
        .map(|parameters| rust_parameter_types(parameters, src))
        .unwrap_or_default();
    let arity = u16::try_from(param_types.len()).unwrap_or(u16::MAX);
    let return_type = node
        .child_by_field_name("return_type")
        .map(|return_type| clean_type(text(return_type, src)));

    let (container, kind, owner_id, id, legacy_qualified_name) = if let Some(owner) = owner {
        let id = method_id(&owner.fqcn, &name, arity);
        (
            owner.fqcn.clone(),
            NodeKind::Method,
            Some(type_id(owner.kind, &owner.fqcn)),
            id,
            format!("{}::{name}", owner.fqcn),
        )
    } else {
        let legacy_fqcn = join_path(module, &name);
        let id = function_id(&legacy_fqcn, &name, arity);
        (
            module.to_string(),
            NodeKind::Function,
            None,
            id,
            legacy_fqcn,
        )
    };
    let callable_scope = format!("{container}#{name}/{arity}");
    let range = range_of(node);
    let def = SymbolDef {
        id: id.clone(),
        kind,
        fqcn: container,
        name: name.clone(),
        owner: owner_id.clone(),
        range,
        modifiers: Vec::new(),
        param_types,
        return_type,
        declared_type: None,
        framework_role: None,
        complexity: None,
        body_fingerprint: None,
        lang_meta: None,
    };
    let graph_node = Node {
        id: id.clone(),
        kind,
        name,
        qualified_name: Some(legacy_qualified_name),
        file: rel.to_string(),
        range,
        props: None,
    };
    let edge = Edge {
        src: owner_id.unwrap_or_else(|| file_id.clone()),
        dst: id,
        kind: if kind == NodeKind::Method {
            EdgeKind::HasMethod
        } else {
            EdgeKind::Contains
        },
        confidence: 1.0,
        reason: "structure".into(),
        props: None,
    };
    Some((def, graph_node, edge, callable_scope))
}

fn extract_type(
    node: TsNode<'_>,
    src: &str,
    rel: &str,
    module: &str,
    file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let name = text(node.child_by_field_name("name")?, src).to_string();
    let kind = if node.kind() == "enum_item" {
        NodeKind::Enum
    } else {
        NodeKind::Class
    };
    extract_named_type(name, kind, node, rel, module, file_id)
}

fn extract_trait(
    node: TsNode<'_>,
    src: &str,
    rel: &str,
    module: &str,
    file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let name = text(node.child_by_field_name("name")?, src).to_string();
    extract_named_type(name, NodeKind::Interface, node, rel, module, file_id)
}

fn extract_named_type(
    name: String,
    kind: NodeKind,
    node: TsNode<'_>,
    rel: &str,
    module: &str,
    file_id: &NodeId,
) -> Option<(SymbolDef, Node, Edge)> {
    let fqcn = join_path(module, &name);
    let id = type_id(kind, &fqcn);
    let range = range_of(node);
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
        name,
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

fn collect_use(node: TsNode<'_>, src: &str, imports: &mut Vec<RawImport>) {
    if let Some(argument) = node.child_by_field_name("argument") {
        flatten_use(argument, src, "", range_of(node), imports);
    }
}

fn flatten_use(
    node: TsNode<'_>,
    src: &str,
    prefix: &str,
    range: Range,
    imports: &mut Vec<RawImport>,
) {
    match node.kind() {
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map(|path| join_path(prefix, text(path, src)))
                .unwrap_or_else(|| prefix.to_string());
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use(list, src, &path, range, imports);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                flatten_use(child, src, prefix, range, imports);
            }
        }
        "use_as_clause" => {
            let Some(path) = node.child_by_field_name("path") else {
                return;
            };
            let alias = node
                .child_by_field_name("alias")
                .map(|alias| text(alias, src).to_string());
            imports.push(RawImport {
                raw: join_path(prefix, text(path, src)),
                is_static: false,
                is_wildcard: false,
                alias,
                range,
            });
        }
        "use_wildcard" => {
            let mut raw = prefix.to_string();
            let mut cursor = node.walk();
            if let Some(path) = node.named_children(&mut cursor).next() {
                raw = join_path(prefix, text(path, src));
            }
            imports.push(RawImport {
                raw,
                is_static: false,
                is_wildcard: true,
                alias: None,
                range,
            });
        }
        "self" if !prefix.is_empty() => imports.push(RawImport {
            raw: prefix.to_string(),
            is_static: false,
            is_wildcard: false,
            alias: None,
            range,
        }),
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" => {
            imports.push(RawImport {
                raw: join_path(prefix, text(node, src)),
                is_static: false,
                is_wildcard: false,
                alias: None,
                range,
            });
        }
        _ => {}
    }
}

fn collect_calls(
    root: TsNode<'_>,
    src: &str,
    in_fqcn: &str,
    in_callable: &NodeId,
    sites: &mut Vec<ReferenceSite>,
) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.id() != root.id()
            && matches!(node.kind(), "function_item" | "function_signature_item")
        {
            continue;
        }
        if node.kind() == "call_expression" {
            if let Some(function) = node.child_by_field_name("function") {
                let (receiver, name) = call_target(function, src);
                if !name.is_empty() {
                    let arguments = node.child_by_field_name("arguments");
                    let arg_texts = arguments
                        .map(|arguments| {
                            let mut cursor = arguments.walk();
                            arguments
                                .named_children(&mut cursor)
                                .map(|argument| text(argument, src).to_string())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    sites.push(ReferenceSite {
                        name,
                        receiver,
                        kind: RefKind::Call,
                        arity: Some(u16::try_from(arg_texts.len()).unwrap_or(u16::MAX)),
                        range: range_of(node),
                        in_fqcn: in_fqcn.to_string(),
                        in_callable: in_callable.clone(),
                        arg_texts,
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

fn call_target(function: TsNode<'_>, src: &str) -> (Option<String>, String) {
    match function.kind() {
        "field_expression" => (
            function
                .child_by_field_name("value")
                .map(|value| text(value, src).to_string()),
            function
                .child_by_field_name("field")
                .map(|field| text(field, src).to_string())
                .unwrap_or_default(),
        ),
        "scoped_identifier" => (
            function
                .child_by_field_name("path")
                .map(|path| text(path, src).to_string()),
            function
                .child_by_field_name("name")
                .map(|name| text(name, src).to_string())
                .unwrap_or_default(),
        ),
        _ => (None, text(function, src).to_string()),
    }
}

fn collect_type_bindings(
    function: TsNode<'_>,
    src: &str,
    callable_scope: &str,
    bindings: &mut Vec<TypeBinding>,
) {
    collect_parameter_bindings(function, src, callable_scope, bindings);
    let mut stack = function
        .child_by_field_name("body")
        .into_iter()
        .collect::<Vec<_>>();
    while let Some(node) = stack.pop() {
        if node.kind() == "let_declaration" {
            if let (Some(pattern), Some(raw_type)) = (
                node.child_by_field_name("pattern"),
                node.child_by_field_name("type"),
            ) {
                if let Some(name) = binding_name(pattern, src) {
                    bindings.push(TypeBinding {
                        name,
                        raw_type: clean_type(text(raw_type, src)),
                        kind: BindingKind::Local,
                        in_fqcn: callable_scope.to_string(),
                        qualifier: None,
                        range: range_of(node),
                    });
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !matches!(child.kind(), "function_item" | "function_signature_item") {
                stack.push(child);
            }
        }
    }
}

fn collect_parameter_bindings(
    function: TsNode<'_>,
    src: &str,
    callable_scope: &str,
    bindings: &mut Vec<TypeBinding>,
) {
    let Some(parameters) = function.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "parameter" {
            continue;
        }
        let (Some(pattern), Some(raw_type)) = (
            parameter.child_by_field_name("pattern"),
            parameter.child_by_field_name("type"),
        ) else {
            continue;
        };
        if let Some(name) = binding_name(pattern, src) {
            bindings.push(TypeBinding {
                name,
                raw_type: clean_type(text(raw_type, src)),
                kind: BindingKind::Param,
                in_fqcn: callable_scope.to_string(),
                qualifier: None,
                range: range_of(parameter),
            });
        }
    }
}

fn rust_parameter_types(parameters: TsNode<'_>, src: &str) -> Vec<String> {
    let mut cursor = parameters.walk();
    parameters
        .named_children(&mut cursor)
        .filter(|parameter| parameter.kind() == "parameter")
        .filter_map(|parameter| parameter.child_by_field_name("type"))
        .map(|raw_type| clean_type(text(raw_type, src)))
        .collect()
}

fn binding_name(pattern: TsNode<'_>, src: &str) -> Option<String> {
    if pattern.kind() == "identifier" {
        return Some(text(pattern, src).to_string());
    }
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" {
            return Some(text(node, src).to_string());
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    None
}

fn clean_type(raw: &str) -> String {
    let mut value = raw.trim();
    while let Some(stripped) = value.strip_prefix('&') {
        value = stripped.trim_start();
        if value.starts_with('\'') {
            value = value
                .split_once(char::is_whitespace)
                .map(|(_, rest)| rest.trim_start())
                .unwrap_or(value);
        }
        value = value.strip_prefix("mut ").unwrap_or(value).trim_start();
    }
    value.to_string()
}

fn qualify_type(module: &str, raw: &str) -> String {
    let raw = clean_type(raw);
    let base = raw.split('<').next().unwrap_or(&raw).trim();
    if let Some(path) = base.strip_prefix("crate::") {
        path.to_string()
    } else if base.contains("::") {
        base.to_string()
    } else {
        join_path(module, base)
    }
}

fn join_path(prefix: &str, suffix: &str) -> String {
    match (prefix.is_empty(), suffix.is_empty()) {
        (_, true) => prefix.to_string(),
        (true, false) => suffix.to_string(),
        (false, false) => format!("{prefix}::{suffix}"),
    }
}
