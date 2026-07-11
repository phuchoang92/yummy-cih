//! Kotlin framework detection — a 1:1 port of the Java detector for
//! Spring MVC routes, Feign client proxies, Kafka/Spring event contracts, and
//! RestTemplate/WebClient outbound HTTP calls. Tree walking is Kotlin-specific
//! (the grammars differ); all string normalization is shared via
//! [`crate::contracts_common`].
//!
//! Phase A scope: literal URL/topic strings only — interpolated strings
//! (`"$base/items"`) yield a site with no `url_template` (Phase B folds them).

use cih_core::{
    ContractKind, ContractSite, Edge, EdgeKind, MessagingFramework, Node, NodeId, NodeKind, Range,
    RouteSource,
};
use tree_sitter::Node as TsNode;

use super::parse::{
    find_named_child, first_simple_identifier, has_child_kind, range_of, text, Builder,
    CallableCtx, TypeCtx,
};
use crate::contracts_common::{
    base_type_simple, infer_webclient_http_method, normalize_external_url, normalize_route_path,
    rest_template_http_method, spring_http_method,
};

pub(super) fn collect(root: TsNode<'_>, src: &str, builder: &mut Builder) {
    match root.kind() {
        "class_declaration" if has_child_kind(root, "interface") => {
            emit_feign_contracts(root, src, builder);
        }
        "function_declaration" => {
            emit_function_routes(root, src, builder);
            emit_listener_contracts(root, src, builder);
        }
        "call_expression" => emit_call_contract(root, src, builder),
        _ => {}
    }

    let mut cursor = root.walk();
    let children: Vec<_> = root.named_children(&mut cursor).collect();
    for child in children {
        collect(child, src, builder);
    }
}

// ── Spring MVC routes ────────────────────────────────────────────────────────

struct FnRoute {
    annotation: String,
    http_method: &'static str,
    path: String,
    range: Range,
}

fn emit_function_routes(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let routes = spring_function_routes(node, src);
    if routes.is_empty() {
        return;
    }
    let Some(callable) = callable_context_at(node.start_byte(), builder) else {
        return;
    };
    let (callable_id, signature) = (callable.id.clone(), callable.signature.clone());
    let prefix = type_context_at(node.start_byte(), builder)
        .and_then(|ctx| ctx.spring_prefix.clone())
        .filter(|p| !p.is_empty())
        .unwrap_or_default();

    for route in routes {
        let path = normalize_route_path(&route.path, &prefix);
        let name = format!("{} {path}", route.http_method);
        let route_id = NodeId::new(format!("Route:{name}"));
        builder.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: builder.rel.clone(),
            range: route.range,
            props: Some(serde_json::json!({
                "httpMethod": route.http_method,
                "path": path,
                "route_annotations": [route.annotation.clone()],
                "source": RouteSource::SpringMvc,
                "handler": signature,
            })),
        });
        builder.edges.push(Edge::new(
            callable_id.clone(),
            route_id,
            EdgeKind::HandlesRoute,
            1.0,
            format!("spring-{}", route.annotation),
        ));
    }
}

fn spring_function_routes(node: TsNode<'_>, src: &str) -> Vec<FnRoute> {
    let mut routes = Vec::new();
    for annotation in annotations(node) {
        let Some(name) = annotation_name(annotation, src) else {
            continue;
        };
        let Some(http_method) = spring_http_method(&name) else {
            continue;
        };
        let paths = annotation_string_values(annotation, src, &["value", "path"]);
        let range = range_of(annotation);
        if paths.is_empty() {
            routes.push(FnRoute {
                annotation: name.clone(),
                http_method,
                path: String::new(),
                range,
            });
        } else {
            for path in paths {
                routes.push(FnRoute {
                    annotation: name.clone(),
                    http_method,
                    path,
                    range,
                });
            }
        }
    }
    routes
}

/// Class-level `@RequestMapping("/prefix")` value; used by the declaration
/// walk to seed each type context's `spring_prefix`.
pub(super) fn spring_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node)
        .into_iter()
        .find(|ann| annotation_name(*ann, src).as_deref() == Some("RequestMapping"))
        .and_then(|ann| {
            annotation_string_values(ann, src, &["value", "path"])
                .into_iter()
                .next()
        })
}

// ── Feign client proxies ─────────────────────────────────────────────────────

fn emit_feign_contracts(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(feign) = annotations(node)
        .into_iter()
        .find(|ann| annotation_name(*ann, src).as_deref() == Some("FeignClient"))
    else {
        return;
    };
    let base = annotation_string_values(feign, src, &["url", "path", "value"])
        .into_iter()
        .next();
    let Some(body) = find_named_child(node, "class_body") else {
        return;
    };

    let mut cursor = body.walk();
    let functions: Vec<_> = body
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "function_declaration")
        .collect();
    for function in functions {
        let Some(callable) = callable_context_at(function.start_byte(), builder) else {
            continue;
        };
        let callable_id = callable.id.clone();
        for route in spring_function_routes(function, src) {
            let url = if let Some(base) = base.as_deref().filter(|base| base.starts_with('/')) {
                normalize_route_path(&route.path, base)
            } else {
                normalize_external_url(&route.path)
            };
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::HttpClientProxy,
                url_template: Some(url),
                topic: None,
                http_method: Some(route.http_method.to_string()),
                messaging_framework: None,
                in_callable: callable_id.clone(),
                range: route.range,
            });
        }
    }
}

// ── Event listeners ──────────────────────────────────────────────────────────

fn emit_listener_contracts(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(callable) = callable_context_at(node.start_byte(), builder) else {
        return;
    };
    let callable_id = callable.id.clone();
    for annotation in annotations(node) {
        match annotation_name(annotation, src).as_deref() {
            Some("KafkaListener") => {
                for topic in
                    annotation_string_values(annotation, src, &["topics", "topic", "value"])
                {
                    builder.contract_sites.push(ContractSite {
                        kind: ContractKind::EventListen,
                        url_template: None,
                        topic: Some(topic),
                        http_method: None,
                        messaging_framework: Some(MessagingFramework::Kafka),
                        in_callable: callable_id.clone(),
                        range: range_of(annotation),
                    });
                }
            }
            Some("EventListener") => {
                if let Some(topic) = first_parameter_type(node, src) {
                    builder.contract_sites.push(ContractSite {
                        kind: ContractKind::EventListen,
                        url_template: None,
                        topic: Some(base_type_simple(&topic)),
                        http_method: None,
                        messaging_framework: Some(MessagingFramework::Spring),
                        in_callable: callable_id.clone(),
                        range: range_of(annotation),
                    });
                }
            }
            _ => {}
        }
    }
}

fn first_parameter_type(function: TsNode<'_>, src: &str) -> Option<String> {
    let params = find_named_child(function, "function_value_parameters")?;
    let param = find_named_child(params, "parameter")?;
    super::parse::declared_type_text(param, src)
}

// ── Outbound calls (RestTemplate / WebClient / KafkaTemplate / publishEvent) ─

fn emit_call_contract(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(nav) = node
        .named_child(0)
        .filter(|child| child.kind() == "navigation_expression")
    else {
        return;
    };
    let Some(suffix) = find_named_child(nav, "navigation_suffix") else {
        return;
    };
    let Some(method) = first_simple_identifier(suffix, src) else {
        return;
    };
    let Some(receiver_node) = nav.named_child(0) else {
        return;
    };
    let receiver = text(receiver_node, src);
    let Some(callable) = callable_context_at(node.start_byte(), builder) else {
        return;
    };
    let (callable_id, signature) = (callable.id.clone(), callable.signature.clone());

    if let Some(http_method) = rest_template_http_method(&method) {
        if receiver_has_type(builder, &signature, &receiver, "RestTemplate") {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::HttpCall,
                url_template: first_string_argument(node, src)
                    .map(|url| normalize_external_url(&url)),
                topic: None,
                http_method: Some(http_method.to_string()),
                messaging_framework: None,
                in_callable: callable_id,
                range: range_of(node),
            });
        }
        return;
    }

    if method == "uri" {
        if let Some(http_method) = infer_webclient_http_method(&receiver) {
            if root_receiver_has_type(builder, &signature, &receiver, "WebClient") {
                builder.contract_sites.push(ContractSite {
                    kind: ContractKind::HttpCall,
                    url_template: first_string_argument(node, src)
                        .map(|url| normalize_external_url(&url)),
                    topic: None,
                    http_method: Some(http_method.to_string()),
                    messaging_framework: None,
                    in_callable: callable_id,
                    range: range_of(node),
                });
            }
        }
        return;
    }

    if method == "send" && receiver_has_type(builder, &signature, &receiver, "KafkaTemplate") {
        if let Some(topic) = first_string_argument(node, src) {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic: Some(topic),
                http_method: None,
                messaging_framework: Some(MessagingFramework::Kafka),
                in_callable: callable_id,
                range: range_of(node),
            });
        }
        return;
    }

    if method == "publishEvent"
        && receiver_has_type(builder, &signature, &receiver, "ApplicationEventPublisher")
    {
        if let Some(topic) = first_constructor_argument_type(node, src) {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic: Some(topic),
                http_method: None,
                messaging_framework: Some(MessagingFramework::Spring),
                in_callable: callable_id,
                range: range_of(node),
            });
        }
    }
}

// ── Receiver typing (light per-class env) ────────────────────────────────────
//
// Matches the receiver's simple name against the type bindings the declaration
// walk collected from primary-constructor parameters and typed properties.
// Heuristic by design: no inheritance, no locals — documented in
// docs/ARCHITECTURE.md.

fn receiver_has_type(builder: &Builder, signature: &str, receiver: &str, expected: &str) -> bool {
    let receiver = receiver.trim();
    if receiver.is_empty() {
        return false;
    }
    let candidate = receiver.rsplit('.').next().unwrap_or(receiver);
    binding_has_type(
        builder,
        signature,
        candidate.trim().trim_end_matches("()"),
        expected,
    )
}

fn root_receiver_has_type(
    builder: &Builder,
    signature: &str,
    receiver: &str,
    expected: &str,
) -> bool {
    let root = receiver
        .split('.')
        .next()
        .unwrap_or(receiver)
        .trim()
        .trim_end_matches("()");
    binding_has_type(builder, signature, root, expected)
}

fn binding_has_type(builder: &Builder, signature: &str, name: &str, expected: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let owner = signature.split('#').next().unwrap_or(signature);
    builder.type_bindings.iter().any(|binding| {
        binding.name == name
            && binding.in_fqcn == owner
            && base_type_simple(&binding.raw_type) == expected
    })
}

// ── Context lookups (byte-offset analogs of the Java parser's) ──────────────

fn callable_context_at(byte: usize, builder: &Builder) -> Option<&CallableCtx> {
    builder
        .callable_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
}

fn type_context_at(byte: usize, builder: &Builder) -> Option<&TypeCtx> {
    builder
        .type_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
}

// ── Annotation helpers (Kotlin grammar shapes) ───────────────────────────────
//
// `@Marker`            → (annotation (user_type (type_identifier)))
// `@Anno("x")`         → (annotation (constructor_invocation (user_type …)
//                          (value_arguments (value_argument (string_literal …)))))
// `@Anno(k = ["a"])`   → value_argument = (simple_identifier) (collection_literal …)

fn annotations(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let Some(modifiers) = find_named_child(node, "modifiers") else {
        return Vec::new();
    };
    let mut cursor = modifiers.walk();
    modifiers
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "annotation")
        .collect()
}

fn annotation_name(annotation: TsNode<'_>, src: &str) -> Option<String> {
    let user_type = find_descendant(annotation, "user_type")?;
    let mut cursor = user_type.walk();
    let mut last = None;
    for child in user_type.named_children(&mut cursor) {
        if child.kind() == "type_identifier" {
            last = Some(text(child, src));
        }
    }
    last.filter(|name| !name.is_empty())
}

/// String values of an annotation for the given member keys. A positional
/// argument counts as key `"value"`. Sorted + deduped (parity with the Java
/// helper).
fn annotation_string_values(annotation: TsNode<'_>, src: &str, keys: &[&str]) -> Vec<String> {
    let Some(arguments) = find_descendant(annotation, "value_arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if argument.kind() != "value_argument" {
            continue;
        }
        let mut inner = argument.walk();
        let children: Vec<_> = argument.named_children(&mut inner).collect();
        let (key, value_nodes) = match children.split_first() {
            Some((first, rest)) if first.kind() == "simple_identifier" && !rest.is_empty() => {
                (text(*first, src), rest.to_vec())
            }
            _ => ("value".to_string(), children),
        };
        if !keys.iter().any(|candidate| *candidate == key) {
            continue;
        }
        for value in value_nodes {
            collect_literal_strings(value, src, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn collect_literal_strings(node: TsNode<'_>, src: &str, out: &mut Vec<String>) {
    if node.kind() == "string_literal" {
        if let Some(value) = literal_string_text(node, src) {
            out.push(value);
        }
        return;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_literal_strings(child, src, out);
    }
}

/// Text of a fully-literal string (no `$id`/`${expr}` interpolation).
fn literal_string_text(node: TsNode<'_>, src: &str) -> Option<String> {
    if node.kind() != "string_literal" {
        return None;
    }
    let mut content = String::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_content" | "character_escape_seq" => {
                content.push_str(child.utf8_text(src.as_bytes()).unwrap_or_default());
            }
            // Interpolation — not a literal in Phase A (Phase B folds parts).
            _ => return None,
        }
    }
    Some(content)
}

// ── Call-argument helpers ────────────────────────────────────────────────────

fn call_value_arguments(call: TsNode<'_>) -> Option<TsNode<'_>> {
    let suffix = find_named_child(call, "call_suffix")?;
    find_named_child(suffix, "value_arguments")
}

/// First fully-literal string argument of a call, unquoted.
fn first_string_argument(call: TsNode<'_>, src: &str) -> Option<String> {
    let arguments = call_value_arguments(call)?;
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if argument.kind() != "value_argument" {
            continue;
        }
        let mut inner = argument.walk();
        for value in argument.named_children(&mut inner) {
            if value.kind() == "string_literal" {
                return literal_string_text(value, src);
            }
        }
    }
    None
}

/// Simple type name of the first constructor-call argument
/// (`publishEvent(OrderPlaced(id))` → `OrderPlaced`).
fn first_constructor_argument_type(call: TsNode<'_>, src: &str) -> Option<String> {
    let arguments = call_value_arguments(call)?;
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if argument.kind() != "value_argument" {
            continue;
        }
        let mut inner = argument.walk();
        for value in argument.named_children(&mut inner) {
            if value.kind() != "call_expression" {
                continue;
            }
            let callee = value
                .named_child(0)
                .filter(|child| child.kind() == "simple_identifier")
                .map(|child| text(child, src))?;
            if callee.chars().next().is_some_and(char::is_uppercase) {
                return Some(base_type_simple(&callee));
            }
        }
    }
    None
}

fn find_descendant<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    for child in children {
        if let Some(found) = find_descendant(child, kind) {
            return Some(found);
        }
    }
    None
}
