use std::collections::BTreeSet;

use anyhow::{Context, Result};
use cih_core::{
    file_id, ContractSite, Edge, Node, NodeId, NodeKind, ParsedFile, ParsedUnit, RawImport, Range,
    ReferenceSite, RouteSource, SqlConstant, SqlExecutionSite, StringConstant, SymbolDef,
    TypeBinding,
};
use tree_sitter::Node as TsNode;

use super::JavaProvider;
use crate::LanguageProvider;

mod constants;
mod declarations;
mod framework;
mod heritage;
mod metrics;
mod normalize;
mod references;
mod structural;

#[derive(Clone, Debug)]
pub(super) struct TypeContext {
    pub(super) id: NodeId,
    pub(super) kind: NodeKind,
    pub(super) fqcn: String,
    pub(super) spring_prefix: Option<String>,
    pub(super) is_test: bool,
    pub(super) start_byte: usize,
    pub(super) end_byte: usize,
}

#[derive(Clone, Debug)]
pub(super) struct CallableContext {
    pub(super) id: NodeId,
    pub(super) in_fqcn: String,
    pub(super) start_byte: usize,
    pub(super) end_byte: usize,
}

#[derive(Clone, Debug)]
pub(super) struct MethodRoute {
    pub(super) annotations: Vec<String>,
    pub(super) http_method: &'static str,
    pub(super) path: String,
    pub(super) range: Range,
    pub(super) source: RouteSource,
}

#[derive(Default)]
pub(super) struct FileBuilder {
    pub(super) file: String,
    pub(super) package: Option<String>,
    pub(super) nodes: Vec<Node>,
    pub(super) edges: Vec<Edge>,
    pub(super) defs: Vec<SymbolDef>,
    pub(super) imports: Vec<RawImport>,
    pub(super) reference_sites: Vec<ReferenceSite>,
    pub(super) type_bindings: Vec<TypeBinding>,
    pub(super) contract_sites: Vec<ContractSite>,
    pub(super) sql_constants: Vec<SqlConstant>,
    pub(super) sql_execution_sites: Vec<SqlExecutionSite>,
    pub(super) string_constants: Vec<StringConstant>,
    pub(super) type_contexts: Vec<TypeContext>,
    pub(super) callable_contexts: Vec<CallableContext>,
}

pub(super) fn parse_java_file(provider: &JavaProvider, rel: &str, src: &str) -> Result<ParsedUnit> {
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

    declarations::collect_declarations(root, src, &mut builder, None);
    references::collect_query_ir(provider, &tree, src, &mut builder);
    heritage::collect_heritage_references(root, src, &mut builder);
    framework::collect_method_routes(root, src, &mut builder);
    framework::collect_contract_sites(root, src, &mut builder);
    constants::collect_sql_constants(root, src, &mut builder);
    constants::collect_sql_execution_sites(root, src, &mut builder);
    constants::collect_static_string_constants(root, src, &mut builder);
    structural::attach_structural_profiles(&mut builder);
    normalize::normalize_builder(&mut builder);

    let import_bindings = builder.imports.iter().map(|imp| {
        use cih_core::{ImportBinding, ImportBindingKind};
        let kind = if imp.is_wildcard {
            ImportBindingKind::Wildcard
        } else if imp.is_static {
            ImportBindingKind::StaticMember
        } else {
            ImportBindingKind::Named
        };
        // Static-member and named imports split identically; only wildcards differ.
        let (module, imported) = if imp.is_wildcard {
            (imp.raw.trim_end_matches(".*").to_string(), None)
        } else if let Some((m, i)) = imp.raw.rsplit_once('.') {
            (m.to_string(), Some(i.to_string()))
        } else {
            (imp.raw.clone(), None)
        };
        ImportBinding { module, imported, local: None, kind, range: imp.range }
    }).collect::<Vec<_>>();

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        import_bindings,
        parsed_file: ParsedFile {
            file: builder.file,
            language: "java".to_string(),
            package: builder.package,
            defs: builder.defs,
            imports: builder.imports,
            reference_sites: builder.reference_sites,
            type_bindings: builder.type_bindings,
            contract_sites: builder.contract_sites,
            sql_constants: builder.sql_constants,
            sql_execution_sites: builder.sql_execution_sites,
            string_constants: builder.string_constants,
        },
    })
}

// ── Shared utility functions (used by 2+ submodules) ────────────────────────

pub(super) fn method_declarations(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let mut out = Vec::new();
    collect_method_declarations(node, &mut out);
    out
}

fn collect_method_declarations<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if node.kind() == "method_declaration" {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_method_declarations(child, out);
    }
}

pub(super) fn annotation_string_values(node: TsNode<'_>, src: &str, keys: &[&str]) -> Vec<String> {
    let Some(arguments) = first_named_child(node, "annotation_argument_list") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "string_literal" => {
                if keys.contains(&"value") {
                    if let Some(value) = unquote_spring_literal(&text(child, src)) {
                        out.push(value);
                    }
                }
            }
            "element_value_array_initializer" | "array_initializer" => {
                if keys.contains(&"value") {
                    for string_node in string_literals(child) {
                        if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                            out.push(value);
                        }
                    }
                }
            }
            "element_value_pair" => {
                let key = child
                    .child_by_field_name("key")
                    .or_else(|| first_named_child(child, "identifier"))
                    .map(|node| text(node, src));
                if !key
                    .as_deref()
                    .is_some_and(|key| keys.iter().any(|candidate| candidate == &key))
                {
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

/// A JSON snapshot of every annotation on a declaration: its simple name plus any string-literal
/// attributes (positional `value` and `key = "..."` pairs). Retained on Method/Class node props so
/// a config-driven pattern engine can match arbitrary/custom annotations post-parse — no hardcoded
/// per-framework handler required.
pub(super) fn annotation_metadata(node: TsNode<'_>, src: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for ann in annotations(node) {
        let Some(name) = annotation_name(ann, src) else {
            continue;
        };
        let mut attrs = serde_json::Map::new();
        if let Some(args) = first_named_child(ann, "annotation_argument_list") {
            let mut cursor = args.walk();
            for child in args.named_children(&mut cursor) {
                match child.kind() {
                    "string_literal" => {
                        if let Some(v) = unquote_spring_literal(&text(child, src)) {
                            attrs
                                .entry("value".to_string())
                                .or_insert(serde_json::Value::String(v));
                        }
                    }
                    "element_value_array_initializer" | "array_initializer" => {
                        let vals: Vec<serde_json::Value> = string_literals(child)
                            .into_iter()
                            .filter_map(|s| unquote_spring_literal(&text(s, src)))
                            .map(serde_json::Value::String)
                            .collect();
                        if !vals.is_empty() {
                            attrs
                                .entry("value".to_string())
                                .or_insert(serde_json::Value::Array(vals));
                        }
                    }
                    "element_value_pair" => {
                        let key = child
                            .child_by_field_name("key")
                            .or_else(|| first_named_child(child, "identifier"))
                            .map(|k| text(k, src))
                            .filter(|k| !k.is_empty());
                        if let Some(key) = key {
                            let mut vals: Vec<String> = string_literals(child)
                                .into_iter()
                                .filter_map(|s| unquote_spring_literal(&text(s, src)))
                                .collect();
                            match vals.len() {
                                0 => {}
                                1 => {
                                    attrs.insert(key, serde_json::Value::String(vals.remove(0)));
                                }
                                _ => {
                                    attrs.insert(
                                        key,
                                        serde_json::Value::Array(
                                            vals.into_iter().map(serde_json::Value::String).collect(),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut entry = serde_json::Map::new();
        entry.insert("name".to_string(), serde_json::Value::String(name));
        if !attrs.is_empty() {
            entry.insert("attrs".to_string(), serde_json::Value::Object(attrs));
        }
        out.push(serde_json::Value::Object(entry));
    }
    out
}

// Pure string helpers shared with other framework detectors (Kotlin) live in
// `contracts_common`; re-exported here so java submodule call sites are unchanged.
pub(crate) use crate::contracts_common::{
    base_type_simple, infer_webclient_http_method, normalize_external_url, normalize_route_path,
    rest_template_http_method, spring_http_method,
};

pub(super) fn first_string_argument(node: TsNode<'_>, src: &str) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "string_literal" {
            return unquote_spring_literal(&text(child, src));
        }
    }
    None
}

pub(super) fn first_constructor_argument_type(node: TsNode<'_>, src: &str) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "object_creation_expression" {
            let raw = child
                .child_by_field_name("type")
                .or_else(|| child.named_child(0))
                .map(|ty| text(ty, src))?;
            return Some(base_type_simple(&raw));
        }
    }
    None
}

pub(super) fn receiver_has_type(
    builder: &FileBuilder,
    in_fqcn: &str,
    receiver: &str,
    expected: &str,
) -> bool {
    let receiver = receiver.trim();
    if receiver.is_empty() {
        return false;
    }
    let candidate = receiver.rsplit('.').next().unwrap_or(receiver);
    binding_has_type(builder, in_fqcn, candidate.trim_end_matches("()"), expected)
}

pub(super) fn root_receiver_has_type(
    builder: &FileBuilder,
    in_fqcn: &str,
    receiver: &str,
    expected: &str,
) -> bool {
    let root = receiver
        .split('.')
        .next()
        .unwrap_or(receiver)
        .trim()
        .trim_end_matches("()");
    binding_has_type(builder, in_fqcn, root, expected)
}

fn binding_has_type(builder: &FileBuilder, in_fqcn: &str, name: &str, expected: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let owner = class_of_signature(in_fqcn);
    builder.type_bindings.iter().any(|binding| {
        binding.name == name
            && (binding.in_fqcn == in_fqcn || binding.in_fqcn == owner)
            && base_type_simple(&binding.raw_type) == expected
    })
}

fn class_of_signature(in_fqcn: &str) -> &str {
    in_fqcn.split('#').next().unwrap_or(in_fqcn)
}

pub(super) fn spring_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("RequestMapping"))
        .and_then(|annotation| first_route_value(annotation, src))
}

pub(super) fn jaxrs_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("Path"))
        .and_then(|annotation| first_route_value(annotation, src))
}

pub(super) fn method_routes(node: TsNode<'_>, src: &str) -> Vec<MethodRoute> {
    let mut routes = spring_method_routes_inner(node, src);
    routes.extend(jaxrs_method_routes_inner(node, src));
    routes.sort_by(|a, b| {
        a.http_method
            .cmp(b.http_method)
            .then(a.path.cmp(&b.path))
    });
    routes.dedup_by(|a, b| a.http_method == b.http_method && a.path == b.path);
    routes
}

pub(super) fn spring_method_routes_inner(node: TsNode<'_>, src: &str) -> Vec<MethodRoute> {
    let mut routes = Vec::new();
    for annotation in annotations(node) {
        let Some(annotation_name) = annotation_name(annotation, src) else {
            continue;
        };
        let Some(http_method) = spring_http_method(&annotation_name) else {
            continue;
        };
        let paths = route_values(annotation, src);
        if paths.is_empty() {
            routes.push(MethodRoute {
                annotations: vec![annotation_name.clone()],
                http_method,
                path: String::new(),
                range: range_of(annotation),
                source: RouteSource::SpringMvc,
            });
        } else {
            for path in paths {
                routes.push(MethodRoute {
                    annotations: vec![annotation_name.clone()],
                    http_method,
                    path,
                    range: range_of(annotation),
                    source: RouteSource::SpringMvc,
                });
            }
        }
    }
    routes
}

fn jaxrs_method_routes_inner(node: TsNode<'_>, src: &str) -> Vec<MethodRoute> {
    let verb = annotations(node)
        .into_iter()
        .find_map(|annotation| {
            annotation_name(annotation, src)
                .as_deref()
                .and_then(jaxrs_http_method)
                .map(|method| (method, annotation))
        });
    let Some((http_method, verb_annotation)) = verb else {
        return Vec::new();
    };

    let path_annotation = annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("Path"));
    let paths = path_annotation
        .map(|annotation| route_values(annotation, src))
        .unwrap_or_default();
    let verb_name = annotation_name(verb_annotation, src).unwrap_or_else(|| http_method.to_string());
    let mut annotation_names = vec![verb_name];
    if path_annotation.is_some() {
        annotation_names.push("Path".to_string());
    }
    annotation_names.sort();

    let range = range_of(verb_annotation);
    if paths.is_empty() {
        vec![MethodRoute {
            annotations: annotation_names,
            http_method,
            path: String::new(),
            range,
            source: RouteSource::JaxRs,
        }]
    } else {
        paths
            .into_iter()
            .map(|path| MethodRoute {
                annotations: annotation_names.clone(),
                http_method,
                path,
                range,
                source: RouteSource::JaxRs,
            })
            .collect()
    }
}

pub(super) fn annotations(node: TsNode<'_>) -> Vec<TsNode<'_>> {
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

pub(super) fn annotation_name(node: TsNode<'_>, src: &str) -> Option<String> {
    node.child_by_field_name("name")
        .or_else(|| first_named_child(node, "identifier"))
        .map(|name| text(name, src))
        .filter(|name| !name.is_empty())
}

fn jaxrs_http_method(annotation: &str) -> Option<&'static str> {
    match annotation {
        "GET" => Some("GET"),
        "POST" => Some("POST"),
        "PUT" => Some("PUT"),
        "DELETE" => Some("DELETE"),
        "PATCH" => Some("PATCH"),
        "HEAD" => Some("HEAD"),
        "OPTIONS" => Some("OPTIONS"),
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

pub(super) fn unquote_spring_literal(raw: &str) -> Option<String> {
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

pub(super) fn base_name_node(node: TsNode<'_>) -> Option<TsNode<'_>> {
    match node.kind() {
        "type_identifier" => Some(node),
        "scoped_type_identifier" => node.named_child(node.named_child_count().saturating_sub(1)),
        "generic_type" => node.named_child(0).and_then(base_name_node),
        _ => None,
    }
}

pub(super) fn parse_import(node: TsNode<'_>, src: &str) -> Option<RawImport> {
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

pub(super) fn type_kind(node: TsNode<'_>) -> Option<NodeKind> {
    match node.kind() {
        "class_declaration" => Some(NodeKind::Class),
        "interface_declaration" => Some(NodeKind::Interface),
        "enum_declaration" => Some(NodeKind::Enum),
        "record_declaration" => Some(NodeKind::Record),
        "annotation_type_declaration" => Some(NodeKind::Annotation),
        _ => None,
    }
}

pub(super) fn type_fqcn(
    package: Option<&str>,
    owner: Option<&TypeContext>,
    simple_name: &str,
) -> String {
    if let Some(owner) = owner {
        return format!("{}.{}", owner.fqcn, simple_name);
    }
    match package {
        Some(package) if !package.is_empty() => format!("{package}.{simple_name}"),
        _ => simple_name.to_string(),
    }
}

pub(super) fn parameter_count(node: TsNode<'_>) -> u16 {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            count = count.saturating_add(1);
        }
    }
    count
}

pub(super) fn param_type_names(node: TsNode<'_>, src: &str) -> Vec<String> {
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

pub(super) fn return_type_name(node: TsNode<'_>, src: &str) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    if ty.kind() == "void_type" {
        return None;
    }
    let raw = text(ty, src);
    (!raw.is_empty() && raw != "void").then_some(raw)
}

pub(super) fn call_arity(node: TsNode<'_>) -> Option<u16> {
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

pub(super) fn capture_arg_texts(node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if matches!(child.kind(), "block_comment" | "line_comment") {
            continue;
        }
        let raw = text(child, src);
        if raw.is_empty() {
            continue;
        }
        let truncated = if raw.len() > 120 {
            format!("{}…", &raw[..120])
        } else {
            raw
        };
        out.push(truncated);
    }
    out
}

pub(super) fn should_emit_field_read(node: TsNode<'_>) -> bool {
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

pub(super) fn context_for(byte: usize, builder: &FileBuilder) -> Option<String> {
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

pub(super) fn callable_context_at(byte: usize, builder: &FileBuilder) -> Option<&CallableContext> {
    builder
        .callable_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
}

pub(super) fn callable_id_for(byte: usize, builder: &FileBuilder) -> NodeId {
    callable_context_at(byte, builder)
        .map(|ctx| ctx.id.clone())
        .or_else(|| type_context_at(byte, builder).map(|ctx| ctx.id.clone()))
        .unwrap_or_else(|| file_id(&builder.file))
}

pub(super) fn type_context_at(byte: usize, builder: &FileBuilder) -> Option<&TypeContext> {
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

pub(super) fn modifiers(node: TsNode<'_>, src: &str) -> Vec<String> {
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

pub(super) fn first_named_child<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    let result = node.named_children(&mut cursor).find(|child| child.kind() == kind);
    result
}

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

pub(super) fn text(node: TsNode<'_>, src: &str) -> String {
    node.utf8_text(src.as_bytes())
        .unwrap_or_default()
        .trim()
        .to_string()
}
