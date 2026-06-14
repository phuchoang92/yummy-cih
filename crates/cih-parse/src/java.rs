use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use cih_core::{
    constructor_id, field_id, file_id, method_id, type_id, BindingKind, Edge, EdgeKind, Node,
    NodeId, NodeKind, ParsedFile, Range, RawImport, RefKind, ReferenceSite, SymbolDef, TypeBinding,
};
use cih_lang::java::JavaProvider;
use cih_lang::LanguageProvider;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node as TsNode, QueryCursor, Tree};

use crate::ParsedUnit;

#[derive(Clone, Debug)]
struct TypeContext {
    id: NodeId,
    kind: NodeKind,
    fqcn: String,
    spring_prefix: Option<String>,
    start_byte: usize,
    end_byte: usize,
}

#[derive(Clone, Debug)]
struct CallableContext {
    id: NodeId,
    in_fqcn: String,
    start_byte: usize,
    end_byte: usize,
}

#[derive(Clone, Debug)]
struct SpringRoute {
    annotation: String,
    http_method: &'static str,
    path: String,
    range: Range,
}

#[derive(Default)]
struct FileBuilder {
    file: String,
    package: Option<String>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    defs: Vec<SymbolDef>,
    imports: Vec<RawImport>,
    reference_sites: Vec<ReferenceSite>,
    type_bindings: Vec<TypeBinding>,
    type_contexts: Vec<TypeContext>,
    callable_contexts: Vec<CallableContext>,
}

pub(crate) fn parse_java_file(rel: &str, src: &str) -> Result<ParsedUnit> {
    let provider = JavaProvider::new();
    // Uses cih-lang's thread-local parser: one parser per rayon worker, reused
    // across the files that worker processes (no per-file parser construction).
    let tree = provider
        .parse(src)
        .with_context(|| format!("failed to parse {rel}"))?;

    let root = tree.root_node();
    let package = provider.package_of(root, src);
    let mut builder = FileBuilder {
        file: rel.to_string(),
        package,
        ..FileBuilder::default()
    };

    collect_declarations(root, src, &mut builder, None);
    collect_query_ir(&provider, &tree, src, &mut builder);
    collect_heritage_references(root, src, &mut builder);
    collect_spring_routes(root, src, &mut builder);
    normalize_builder(&mut builder);

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        parsed_file: ParsedFile {
            file: builder.file,
            package: builder.package,
            defs: builder.defs,
            imports: builder.imports,
            reference_sites: builder.reference_sites,
            type_bindings: builder.type_bindings,
        },
    })
}

fn collect_declarations(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner: Option<TypeContext>,
) {
    if let Some(kind) = type_kind(node) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let simple_name = text(name_node, src);
        if simple_name.is_empty() {
            return;
        }
        let fqcn = type_fqcn(builder.package.as_deref(), owner.as_ref(), &simple_name);
        let id = type_id(kind, &fqcn);
        let range = range_of(node);
        let owner_id = owner.as_ref().map(|owner| owner.id.clone());

        builder.nodes.push(Node {
            id: id.clone(),
            kind,
            name: simple_name.clone(),
            qualified_name: Some(fqcn.clone()),
            file: builder.file.clone(),
            range,
            props: build_class_props(node, src),
        });
        builder.defs.push(SymbolDef {
            id: id.clone(),
            kind,
            fqcn: fqcn.clone(),
            name: simple_name,
            owner: owner_id.clone(),
            range,
            modifiers: modifiers(node, src),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
        });

        if let Some(parent_id) = owner_id {
            builder.edges.push(Edge {
                src: parent_id,
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "nested-type".into(),
            });
        } else {
            builder.edges.push(Edge {
                src: file_id(&builder.file),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-type".into(),
            });
        }

        let context = TypeContext {
            id,
            kind,
            fqcn,
            spring_prefix: spring_class_prefix(node, src),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        };
        builder.type_contexts.push(context.clone());
        walk_named_children(node, src, builder, Some(context));
        return;
    }

    if let Some(owner) = owner.as_ref() {
        match node.kind() {
            "method_declaration" => collect_method(node, src, builder, owner),
            "constructor_declaration" => collect_constructor(node, src, builder, owner),
            "field_declaration" => collect_fields(node, src, builder, owner),
            // TODO(phase-3 gap): enum constants (`enum_constant`) and record header
            // components (`formal_parameter` in a `record_declaration`) are not yet
            // emitted as Field members. Acceptable for structure; revisit if context
            // queries need them.
            _ => {}
        }
    }

    walk_named_children(node, src, builder, owner);
}

fn walk_named_children(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner: Option<TypeContext>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_declarations(child, src, builder, owner.clone());
    }
}

fn collect_method(node: TsNode<'_>, src: &str, builder: &mut FileBuilder, owner: &TypeContext) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, src);
    if name.is_empty() {
        return;
    }
    let arity = parameter_count(node);
    let id = method_id(&owner.fqcn, &name, arity);
    let range = range_of(node);
    builder.nodes.push(Node {
        id: id.clone(),
        kind: NodeKind::Method,
        name: name.clone(),
        qualified_name: Some(format!("{}#{name}/{arity}", owner.fqcn)),
        file: builder.file.clone(),
        range,
        props: if is_bean_method(node, src) {
            Some(serde_json::json!({ "isBean": true }))
        } else {
            None
        },
    });
    builder.edges.push(Edge {
        src: owner.id.clone(),
        dst: id.clone(),
        kind: EdgeKind::HasMethod,
        confidence: 1.0,
        reason: "member".into(),
    });
    builder.defs.push(SymbolDef {
        id: id.clone(),
        kind: NodeKind::Method,
        fqcn: owner.fqcn.clone(),
        name: name.clone(),
        owner: Some(owner.id.clone()),
        range,
        modifiers: modifiers(node, src),
        param_types: param_type_names(node, src),
        return_type: return_type_name(node, src),
        declared_type: None,
    });
    builder.callable_contexts.push(CallableContext {
        id: id.clone(),
        in_fqcn: format!("{}#{name}/{arity}", owner.fqcn),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    });
}

fn collect_constructor(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner: &TypeContext,
) {
    let arity = parameter_count(node);
    let id = constructor_id(&owner.fqcn, arity);
    let range = range_of(node);
    builder.nodes.push(Node {
        id: id.clone(),
        kind: NodeKind::Constructor,
        name: "<init>".into(),
        qualified_name: Some(format!("{}#<init>/{arity}", owner.fqcn)),
        file: builder.file.clone(),
        range,
        props: None,
    });
    builder.edges.push(Edge {
        src: owner.id.clone(),
        dst: id.clone(),
        kind: EdgeKind::HasMethod,
        confidence: 1.0,
        reason: "member".into(),
    });
    builder.defs.push(SymbolDef {
        id: id.clone(),
        kind: NodeKind::Constructor,
        fqcn: owner.fqcn.clone(),
        name: "<init>".into(),
        owner: Some(owner.id.clone()),
        range,
        modifiers: modifiers(node, src),
        param_types: param_type_names(node, src),
        return_type: None,
        declared_type: None,
    });
    builder.callable_contexts.push(CallableContext {
        id: id.clone(),
        in_fqcn: format!("{}#<init>/{arity}", owner.fqcn),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    });
}

fn collect_fields(node: TsNode<'_>, src: &str, builder: &mut FileBuilder, owner: &TypeContext) {
    let declared_type = node.child_by_field_name("type").map(|ty| text(ty, src));
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, src);
        if name.is_empty() {
            continue;
        }
        let id = field_id(&owner.fqcn, &name);
        let range = range_of(child);
        builder.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Field,
            name: name.clone(),
            qualified_name: Some(format!("{}#{name}", owner.fqcn)),
            file: builder.file.clone(),
            range,
            props: None,
        });
        builder.edges.push(Edge {
            src: owner.id.clone(),
            dst: id.clone(),
            kind: EdgeKind::HasField,
            confidence: 1.0,
            reason: "member".into(),
        });
        builder.defs.push(SymbolDef {
            id,
            kind: NodeKind::Field,
            fqcn: owner.fqcn.clone(),
            name,
            owner: Some(owner.id.clone()),
            range,
            modifiers: modifiers(node, src),
            param_types: Vec::new(),
            return_type: None,
            declared_type: declared_type.clone(),
        });
    }
}

fn collect_query_ir(provider: &JavaProvider, tree: &Tree, src: &str, builder: &mut FileBuilder) {
    let mut cursor = QueryCursor::new();
    let query = provider.scope_query();
    let capture_names = query.capture_names();
    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());

    while let Some(query_match) = matches.next() {
        let mut captures: BTreeMap<String, TsNode<'_>> = BTreeMap::new();
        for capture in query_match.captures {
            let name = capture_names[capture.index as usize].to_string();
            captures.entry(name).or_insert(capture.node);
        }

        if let Some(import_node) = captures.get("import.statement").copied() {
            if import_node.kind() == "import_declaration" {
                if let Some(import) = parse_import(import_node, src) {
                    builder.imports.push(import);
                }
            }
            continue;
        }

        if let Some(binding) = type_binding(&captures, src, builder) {
            builder.type_bindings.push(binding);
            continue;
        }

        if let Some(site) = reference_site(&captures, src, builder) {
            builder.reference_sites.push(site);
        }
    }
}

fn reference_site(
    captures: &BTreeMap<String, TsNode<'_>>,
    src: &str,
    builder: &FileBuilder,
) -> Option<ReferenceSite> {
    let anchor = reference_anchor(captures)?;
    let name_node = captures
        .get("reference.name")
        .copied()
        .unwrap_or(anchor.node);
    let name = text(name_node, src);
    if name.is_empty() {
        return None;
    }

    if anchor.kind == RefKind::Call
        && anchor.tag == "reference.call.free"
        && anchor.node.child_by_field_name("object").is_some()
    {
        return None;
    }
    if anchor.kind == RefKind::FieldRead && !should_emit_field_read(anchor.node) {
        return None;
    }

    let receiver = captures
        .get("reference.receiver")
        .map(|node| text(*node, src))
        .filter(|value| !value.is_empty());
    let arity = match anchor.kind {
        RefKind::Call | RefKind::Ctor => call_arity(anchor.node),
        _ => None,
    };
    let in_fqcn = context_for(anchor.node.start_byte(), builder).unwrap_or_default();
    let in_callable = callable_id_for(anchor.node.start_byte(), builder);

    Some(ReferenceSite {
        name,
        receiver,
        kind: anchor.kind,
        arity,
        range: range_of(name_node),
        in_fqcn,
        in_callable,
    })
}

#[derive(Clone, Copy)]
struct ReferenceAnchor<'a> {
    tag: &'a str,
    node: TsNode<'a>,
    kind: RefKind,
}

fn reference_anchor<'a>(captures: &'a BTreeMap<String, TsNode<'a>>) -> Option<ReferenceAnchor<'a>> {
    if let Some(node) = captures.get("reference.call.constructor").copied() {
        return Some(ReferenceAnchor {
            tag: "reference.call.constructor",
            node,
            kind: RefKind::Ctor,
        });
    }
    if let Some((tag, node)) = captures
        .iter()
        .find(|(tag, _)| tag.starts_with("reference.call."))
    {
        return Some(ReferenceAnchor {
            tag,
            node: *node,
            kind: RefKind::Call,
        });
    }
    if let Some(node) = captures.get("reference.write.member").copied() {
        return Some(ReferenceAnchor {
            tag: "reference.write.member",
            node,
            kind: RefKind::FieldWrite,
        });
    }
    if let Some(node) = captures.get("reference.read.member").copied() {
        return Some(ReferenceAnchor {
            tag: "reference.read.member",
            node,
            kind: RefKind::FieldRead,
        });
    }
    None
}

/// Build a `TypeBinding` from a `@type-binding.*` query match (params, locals,
/// fields, `var` inference, patterns, aliases). The anchor capture's tag (and, for
/// `.annotation`, the anchor node kind) determines the `BindingKind`.
fn type_binding(
    captures: &BTreeMap<String, TsNode<'_>>,
    src: &str,
    builder: &FileBuilder,
) -> Option<TypeBinding> {
    let (anchor_tag, anchor_node) = captures.iter().find(|(key, _)| {
        let key = key.as_str();
        key.starts_with("type-binding.") && key != "type-binding.type" && key != "type-binding.name"
    })?;
    let type_node = captures.get("type-binding.type")?;
    let name_node = captures.get("type-binding.name")?;
    let raw_type = text(*type_node, src);
    let name = text(*name_node, src);
    if raw_type.is_empty() || name.is_empty() {
        return None;
    }
    Some(TypeBinding {
        name,
        raw_type,
        kind: binding_kind(anchor_tag.as_str(), *anchor_node),
        in_fqcn: context_for(anchor_node.start_byte(), builder).unwrap_or_default(),
        range: range_of(*name_node),
    })
}

fn binding_kind(tag: &str, anchor: TsNode<'_>) -> BindingKind {
    match tag {
        "type-binding.parameter" => BindingKind::Param,
        "type-binding.call-result" => BindingKind::CallResult,
        "type-binding.alias" => BindingKind::Alias,
        // `var x = new User();` — concrete inferred local.
        "type-binding.constructor" => BindingKind::Local,
        "type-binding.return" => BindingKind::Return,
        "type-binding.pattern" => BindingKind::Pattern,
        // `.annotation` covers fields AND locals/enhanced-for — the anchor node
        // kind disambiguates (field_declaration → Field, otherwise Local).
        "type-binding.annotation" => match anchor.kind() {
            "field_declaration" => BindingKind::Field,
            _ => BindingKind::Local,
        },
        _ => BindingKind::Local,
    }
}

fn collect_heritage_references(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    match node.kind() {
        "class_declaration" => {
            let (in_fqcn, owner_id) = heritage_owner(node, builder);
            if let Some(superclass) = node.child_by_field_name("superclass") {
                emit_heritage_from_children(
                    superclass,
                    src,
                    builder,
                    RefKind::Extends,
                    &in_fqcn,
                    &owner_id,
                );
            }
            if let Some(interfaces) = node.child_by_field_name("interfaces") {
                emit_heritage_type_list(
                    interfaces,
                    src,
                    builder,
                    RefKind::Implements,
                    &in_fqcn,
                    &owner_id,
                );
            }
        }
        "interface_declaration" => {
            let (in_fqcn, owner_id) = heritage_owner(node, builder);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "extends_interfaces" {
                    emit_heritage_type_list(
                        child,
                        src,
                        builder,
                        RefKind::Extends,
                        &in_fqcn,
                        &owner_id,
                    );
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_heritage_references(child, src, builder);
    }
}

/// The enclosing type's `(fqcn, node id)` for a heritage clause — the EXTENDS/
/// IMPLEMENTS edge source. Cloned up front so the immutable index borrow ends
/// before the emit functions borrow `builder` mutably.
fn heritage_owner(node: TsNode<'_>, builder: &FileBuilder) -> (String, NodeId) {
    match type_context_at(node.start_byte(), builder) {
        Some(ctx) => (ctx.fqcn.clone(), ctx.id.clone()),
        None => (String::new(), file_id(&builder.file)),
    }
}

fn emit_heritage_type_list(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    kind: RefKind,
    in_fqcn: &str,
    owner_id: &NodeId,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_list" {
            emit_heritage_from_children(child, src, builder, kind, in_fqcn, owner_id);
        } else if let Some(name_node) = base_name_node(child) {
            emit_heritage_reference(name_node, src, builder, kind, in_fqcn, owner_id);
        }
    }
}

fn emit_heritage_from_children(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    kind: RefKind,
    in_fqcn: &str,
    owner_id: &NodeId,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name_node) = base_name_node(child) {
            emit_heritage_reference(name_node, src, builder, kind, in_fqcn, owner_id);
        }
    }
}

fn emit_heritage_reference(
    name_node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    kind: RefKind,
    in_fqcn: &str,
    owner_id: &NodeId,
) {
    let name = text(name_node, src);
    if name.is_empty() {
        return;
    }
    builder.reference_sites.push(ReferenceSite {
        name,
        receiver: None,
        kind,
        arity: None,
        range: range_of(name_node),
        in_fqcn: in_fqcn.to_string(),
        in_callable: owner_id.clone(),
    });
}

fn collect_spring_routes(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    if node.kind() == "method_declaration" {
        emit_spring_routes_for_method(node, src, builder);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_spring_routes(child, src, builder);
    }
}

fn emit_spring_routes_for_method(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let routes = spring_method_routes(node, src);
    if routes.is_empty() {
        return;
    }

    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    let prefix = type_context_at(node.start_byte(), builder)
        .and_then(|ctx| ctx.spring_prefix.as_deref())
        .unwrap_or("")
        .to_string();

    for route in routes {
        let path = normalize_route_path(&route.path, &prefix);
        let name = format!("{} {path}", route.http_method);
        let route_id = NodeId::new(format!("Route:{name}"));
        builder.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: builder.file.clone(),
            range: route.range,
            props: Some(serde_json::json!({
                "httpMethod": route.http_method,
                "path": path,
                "decorator": route.annotation,
                "handler": callable.in_fqcn,
            })),
        });
        builder.edges.push(Edge {
            src: callable.id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("spring-{}", route.annotation),
        });
    }
}

fn spring_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("RequestMapping"))
        .and_then(|annotation| first_route_value(annotation, src))
}

fn spring_method_routes(node: TsNode<'_>, src: &str) -> Vec<SpringRoute> {
    let mut routes = Vec::new();
    for annotation in annotations(node) {
        let Some(annotation_name) = annotation_name(annotation, src) else {
            continue;
        };
        let Some(http_method) = spring_http_method(&annotation_name) else {
            continue;
        };
        for path in route_values(annotation, src) {
            routes.push(SpringRoute {
                annotation: annotation_name.clone(),
                http_method,
                path,
                range: range_of(annotation),
            });
        }
    }
    routes
}

fn annotations(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let Some(modifiers) = first_named_child(node, "modifiers") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_annotations(modifiers, &mut out);
    out
}

fn collect_annotations<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if matches!(node.kind(), "annotation" | "marker_annotation") {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_annotations(child, out);
    }
}

fn annotation_name(node: TsNode<'_>, src: &str) -> Option<String> {
    node.child_by_field_name("name")
        .or_else(|| first_named_child(node, "identifier"))
        .map(|name| text(name, src))
        .filter(|name| !name.is_empty())
}

fn spring_http_method(annotation: &str) -> Option<&'static str> {
    match annotation {
        "GetMapping" => Some("GET"),
        "PostMapping" => Some("POST"),
        "PutMapping" => Some("PUT"),
        "DeleteMapping" => Some("DELETE"),
        "PatchMapping" => Some("PATCH"),
        // `@RequestMapping` is a class prefix here. Method-level forms need
        // `method = RequestMethod.X`, which Phase 3 intentionally skips.
        _ => None,
    }
}

fn first_route_value(annotation: TsNode<'_>, src: &str) -> Option<String> {
    route_values(annotation, src).into_iter().next()
}

fn route_values(annotation: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(arguments) = first_named_child(annotation, "annotation_argument_list") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "string_literal" => {
                if let Some(value) = unquote_spring_literal(&text(child, src)) {
                    out.push(value);
                }
            }
            // Positional array: @GetMapping({"/a", "/b"})
            "element_value_array_initializer" | "array_initializer" => {
                for string_node in string_literals(child) {
                    if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                        out.push(value);
                    }
                }
            }
            "element_value_pair" => {
                let key = child
                    .child_by_field_name("key")
                    .or_else(|| first_named_child(child, "identifier"))
                    .map(|node| text(node, src));
                if !is_route_member_key(key.as_deref()) {
                    continue;
                }
                for string_node in string_literals(child) {
                    if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                        out.push(value);
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn string_literals(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let mut out = Vec::new();
    collect_string_literals(node, &mut out);
    out
}

fn collect_string_literals<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if node.kind() == "string_literal" {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_string_literals(child, out);
    }
}

fn is_route_member_key(key: Option<&str>) -> bool {
    key.is_none_or(|key| key == "path" || key == "value")
}

fn unquote_spring_literal(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if (raw.starts_with("\"\"\"") && raw.ends_with("\"\"\""))
        || (raw.starts_with("'''") && raw.ends_with("'''"))
    {
        return Some(raw[3..raw.len().saturating_sub(3)].to_string());
    }

    let mut chars = raw.chars();
    let first = chars.next()?;
    let last = raw.chars().next_back()?;
    if matches!(first, '"' | '\'' | '`') && first == last && raw.len() >= 2 {
        let start = first.len_utf8();
        let end = raw.len() - last.len_utf8();
        return Some(raw[start..end].to_string());
    }
    Some(raw.to_string())
}

fn normalize_route_path(route_path: &str, prefix: &str) -> String {
    let path_part = route_path.trim().trim_matches('/');
    let prefix_part = prefix.trim().trim_matches('/');
    let joined = if prefix_part.is_empty() {
        format!("/{path_part}")
    } else if path_part.is_empty() {
        format!("/{prefix_part}")
    } else {
        format!("/{prefix_part}/{path_part}")
    };
    collapse_slashes(&joined)
}

fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut previous_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !previous_slash {
                out.push(ch);
            }
            previous_slash = true;
        } else {
            out.push(ch);
            previous_slash = false;
        }
    }
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}

fn base_name_node(node: TsNode<'_>) -> Option<TsNode<'_>> {
    match node.kind() {
        "type_identifier" => Some(node),
        "scoped_type_identifier" => node.named_child(node.named_child_count().saturating_sub(1)),
        "generic_type" => node.named_child(0).and_then(base_name_node),
        _ => None,
    }
}

fn parse_import(node: TsNode<'_>, src: &str) -> Option<RawImport> {
    let raw_text = text(node, src);
    let mut body = raw_text.trim();
    body = body.strip_prefix("import")?.trim();
    let is_static = body.starts_with("static ");
    if is_static {
        body = body.strip_prefix("static")?.trim();
    }
    body = body.trim_end_matches(';').trim();
    if body.is_empty() {
        return None;
    }
    Some(RawImport {
        raw: body.to_string(),
        is_static,
        is_wildcard: body.ends_with(".*"),
        range: range_of(node),
    })
}

fn type_kind(node: TsNode<'_>) -> Option<NodeKind> {
    match node.kind() {
        "class_declaration" => Some(NodeKind::Class),
        "interface_declaration" => Some(NodeKind::Interface),
        "enum_declaration" => Some(NodeKind::Enum),
        "record_declaration" => Some(NodeKind::Record),
        "annotation_type_declaration" => Some(NodeKind::Annotation),
        _ => None,
    }
}

fn type_fqcn(package: Option<&str>, owner: Option<&TypeContext>, simple_name: &str) -> String {
    if let Some(owner) = owner {
        return format!("{}.{}", owner.fqcn, simple_name);
    }
    match package {
        Some(package) if !package.is_empty() => format!("{package}.{simple_name}"),
        _ => simple_name.to_string(),
    }
}

fn parameter_count(node: TsNode<'_>) -> u16 {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        // `receiver_parameter` (`void m(Foo this, ...)`) is NOT an argument callers
        // pass — counting it would make the method-id arity off-by-one versus call
        // sites (which count arguments), silently breaking Phase-4 resolution.
        if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Raw (unresolved) parameter type names for a method/constructor, ordered, with
/// the same `formal_parameter | spread_parameter` filter as `parameter_count` (so
/// `param_types.len()` matches arity; the explicit receiver is excluded).
fn param_type_names(node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        if let Some(ty) = child
            .child_by_field_name("type")
            .or_else(|| child.named_child(0))
        {
            out.push(text(ty, src));
        }
    }
    out
}

/// Raw return type name for a method (`None` for `void` / no type field).
fn return_type_name(node: TsNode<'_>, src: &str) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    if ty.kind() == "void_type" {
        return None;
    }
    let raw = text(ty, src);
    (!raw.is_empty() && raw != "void").then_some(raw)
}

fn call_arity(node: TsNode<'_>) -> Option<u16> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut count = 0u16;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "block_comment" || child.kind() == "line_comment" {
            continue;
        }
        count = count.saturating_add(1);
    }
    Some(count)
}

fn should_emit_field_read(node: TsNode<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    if parent.kind() != "assignment_expression" {
        return true;
    }
    parent
        .child_by_field_name("left")
        .is_none_or(|left| !same_node(left, node))
}

fn same_node(a: TsNode<'_>, b: TsNode<'_>) -> bool {
    a.kind() == b.kind() && a.start_byte() == b.start_byte() && a.end_byte() == b.end_byte()
}

fn context_for(byte: usize, builder: &FileBuilder) -> Option<String> {
    builder
        .callable_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
        .map(|ctx| ctx.in_fqcn.clone())
        .or_else(|| {
            type_context_at(byte, builder)
                .map(|ctx| ctx.fqcn.clone())
                .or_else(|| builder.package.clone())
        })
}

fn callable_context_at(byte: usize, builder: &FileBuilder) -> Option<&CallableContext> {
    builder
        .callable_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
}

/// The graph node id of the callable enclosing `byte` (the edge SOURCE for a
/// reference). Falls back to the enclosing type, then the file — never dangles.
fn callable_id_for(byte: usize, builder: &FileBuilder) -> NodeId {
    callable_context_at(byte, builder)
        .map(|ctx| ctx.id.clone())
        .or_else(|| type_context_at(byte, builder).map(|ctx| ctx.id.clone()))
        .unwrap_or_else(|| file_id(&builder.file))
}

fn type_context_at(byte: usize, builder: &FileBuilder) -> Option<&TypeContext> {
    builder
        .type_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| (ctx.start_byte, type_kind_rank(ctx.kind)))
}

fn type_kind_rank(kind: NodeKind) -> usize {
    match kind {
        NodeKind::Class => 5,
        NodeKind::Interface => 4,
        NodeKind::Enum => 3,
        NodeKind::Record => 2,
        NodeKind::Annotation => 1,
        _ => 0,
    }
}

fn modifiers(node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(modifier_node) = first_named_child(node, "modifiers") else {
        return Vec::new();
    };
    let raw = text(modifier_node, src);
    let known = [
        "public",
        "protected",
        "private",
        "abstract",
        "static",
        "final",
        "native",
        "synchronized",
        "transient",
        "volatile",
        "strictfp",
        "sealed",
        "non-sealed",
    ];
    let known_set = known.into_iter().collect::<BTreeSet<_>>();
    raw.split(|c: char| !c.is_ascii_alphabetic() && c != '-')
        .filter(|part| known_set.contains(part))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The class's specific stereotype, read from its OWN annotations only (not the body,
/// so a `@Service` with a `@GetMapping` method is not mistaken for a controller).
/// First matching annotation wins.
fn class_stereotype(node: TsNode<'_>, src: &str) -> Option<&'static str> {
    for annotation in annotations(node) {
        let mapped = match annotation_name(annotation, src).as_deref() {
            Some("RestController") | Some("Controller") => "controller",
            Some("Service") => "service",
            Some("Repository") => "repository",
            Some("Configuration") => "configuration",
            Some("Component") => "component",
            Some("Entity") => "entity",
            Some("Path") => "resource", // JAX-RS
            _ => continue,
        };
        return Some(mapped);
    }
    None
}

fn is_bean_method(node: TsNode<'_>, src: &str) -> bool {
    annotations(node)
        .into_iter()
        .any(|ann| annotation_name(ann, src).as_deref() == Some("Bean"))
}

const JPA_INTERFACES: &[&str] = &[
    "JpaRepository",
    "CrudRepository",
    "PagingAndSortingRepository",
    "ListCrudRepository",
    "ListPagingAndSortingRepository",
    "MongoRepository",
    "ReactiveCrudRepository",
    "ReactiveMongoRepository",
    "R2dbcRepository",
    "JpaSpecificationExecutor",
];

/// Returns `(is_jpa_repo, entity_type_short_name)`.
/// Checks the `implements` clause for known Spring Data interfaces.
fn jpa_repository_props(node: TsNode<'_>, src: &str) -> (bool, Option<String>) {
    let Some(interfaces_node) = node.child_by_field_name("interfaces") else {
        return (false, None);
    };
    // super_interfaces → type_list → (type_identifier | generic_type)*
    let scan_node = first_named_child(interfaces_node, "interface_type_list")
        .or_else(|| first_named_child(interfaces_node, "type_list"))
        .unwrap_or(interfaces_node);
    let mut cursor = scan_node.walk();
    for child in scan_node.named_children(&mut cursor) {
        match child.kind() {
            "type_identifier" => {
                let name = text(child, src);
                if JPA_INTERFACES.contains(&name.as_str()) {
                    return (true, None);
                }
            }
            "generic_type" => {
                // generic_type has no "name" field; the type identifier is the first named child.
                let Some(name_node) = child.named_child(0) else {
                    continue;
                };
                let name = text(name_node, src);
                if JPA_INTERFACES.contains(&name.as_str()) {
                    // generic_type: [type_identifier, type_arguments]; type_arguments has no field name.
                    // Use named_child(1) to access type_arguments, then named_child(0) for entity type.
                    let entity = child
                        .named_child(1)
                        .and_then(|args| args.named_child(0))
                        .map(|c| text(c, src))
                        .filter(|s| !s.is_empty());
                    return (true, entity);
                }
            }
            _ => {}
        }
    }
    (false, None)
}

fn build_class_props(node: TsNode<'_>, src: &str) -> Option<serde_json::Value> {
    let stereotype = class_stereotype(node, src);
    let (is_jpa, entity_opt) = jpa_repository_props(node, src);
    let effective_stereotype = stereotype.or(if is_jpa { Some("repository") } else { None });
    match (effective_stereotype, entity_opt) {
        (None, None) => None,
        (Some(s), None) => Some(serde_json::json!({ "stereotype": s })),
        (None, Some(e)) => Some(serde_json::json!({ "entityType": e })),
        (Some(s), Some(e)) => Some(serde_json::json!({ "stereotype": s, "entityType": e })),
    }
}

fn first_named_child<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

fn normalize_builder(builder: &mut FileBuilder) {
    builder
        .nodes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    builder.nodes.dedup_by(|a, b| a.id == b.id);
    builder.edges.sort_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then(a.dst.as_str().cmp(b.dst.as_str()))
            .then(a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    builder
        .edges
        .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
    builder.defs.sort_by(|a, b| {
        a.id.as_str()
            .cmp(b.id.as_str())
            .then(a.range.start_line.cmp(&b.range.start_line))
    });
    builder.defs.dedup_by(|a, b| a.id == b.id);
    builder.imports.sort_by(|a, b| {
        a.range
            .start_line
            .cmp(&b.range.start_line)
            .then(a.raw.cmp(&b.raw))
    });
    builder.imports.dedup_by(|a, b| {
        a.raw == b.raw
            && a.is_static == b.is_static
            && a.is_wildcard == b.is_wildcard
            && a.range == b.range
    });
    builder.reference_sites.sort_by(|a, b| {
        a.range
            .start_line
            .cmp(&b.range.start_line)
            .then(a.range.start_col.cmp(&b.range.start_col))
            .then(a.name.cmp(&b.name))
            .then(a.kind_key().cmp(b.kind_key()))
    });
    builder.reference_sites.dedup_by(|a, b| {
        a.name == b.name
            && a.receiver == b.receiver
            && a.kind == b.kind
            && a.arity == b.arity
            && a.range == b.range
            && a.in_fqcn == b.in_fqcn
    });
    builder.type_bindings.sort_by(|a, b| {
        a.in_fqcn
            .cmp(&b.in_fqcn)
            .then(a.name.cmp(&b.name))
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.start_col.cmp(&b.range.start_col))
            .then(a.raw_type.cmp(&b.raw_type))
    });
    builder.type_bindings.dedup_by(|a, b| {
        a.name == b.name
            && a.raw_type == b.raw_type
            && a.kind == b.kind
            && a.in_fqcn == b.in_fqcn
            && a.range == b.range
    });
}

trait RefKindKey {
    fn kind_key(&self) -> &'static str;
}

impl RefKindKey for ReferenceSite {
    fn kind_key(&self) -> &'static str {
        match self.kind {
            RefKind::Call => "call",
            RefKind::FieldRead => "field-read",
            RefKind::FieldWrite => "field-write",
            RefKind::Ctor => "ctor",
            RefKind::Extends => "extends",
            RefKind::Implements => "implements",
            RefKind::TypeRef => "type-ref",
        }
    }
}

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
