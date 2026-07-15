//! Backend HTTP route detection — NestJS/Express/Fastify/Koa/Hapi verb calls
//! and config-object routes (`app.get(path, …)`, `server.route({method,path})`),
//! plus the GraphQL operation mapping. Each becomes a `Route` node + handler edge.

use cih_core::RouteSource;
use tree_sitter::Node as TsNode;


use super::builder::Builder;
use super::helpers::*;

// ── NestJS HTTP verb detection ────────────────────────────────────────────────

pub(super) fn nestjs_http_method(decorator_name: &str) -> Option<&'static str> {
    match decorator_name {
        "Get" => Some("GET"),
        "Post" => Some("POST"),
        "Put" => Some("PUT"),
        "Delete" => Some("DELETE"),
        "Patch" => Some("PATCH"),
        "Head" => Some("HEAD"),
        "Options" => Some("OPTIONS"),
        "All" => Some("ALL"),
        _ => None,
    }
}

// ── Express route detection ───────────────────────────────────────────────────

pub(super) fn express_http_method(method: &str) -> Option<&'static str> {
    match method {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        _ => None,
    }
}

// ── Additional backend route frameworks (Fastify / Koa / Hapi) ─────────────────

/// Lower-case HTTP verb → canonical method, covering the broader set Fastify/Koa
/// expose (`head`/`options`/`all`) on top of Express's five.
pub(super) fn any_http_verb(method: &str) -> Option<&'static str> {
    match method {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        "head" => Some("HEAD"),
        "options" => Some("OPTIONS"),
        "all" => Some("ALL"),
        _ => None,
    }
}

/// Short id-prefix label for a route source (route ids are opaque — matching keys
/// off props, per docs/ARCHITECTURE.md — but a stable label keeps ids readable).
pub(super) fn route_source_label(source: RouteSource) -> &'static str {
    match source {
        RouteSource::Express => "express",
        RouteSource::NestJs => "nestjs",
        RouteSource::Fastify => "fastify",
        RouteSource::Koa => "koa",
        RouteSource::Hapi => "hapi",
        RouteSource::NextJs => "nextjs",
        RouteSource::Remix => "remix",
        RouteSource::GraphQl => "graphql",
        RouteSource::Trpc => "trpc",
        _ => "route",
    }
}

/// GraphQL resolver operation from a `@Query`/`@Mutation`/`@Subscription`
/// decorator (type-graphql / NestJS).
pub(super) fn graphql_operation(dname: &str) -> Option<&'static str> {
    match dname {
        "Query" => Some("QUERY"),
        "Mutation" => Some("MUTATION"),
        "Subscription" => Some("SUBSCRIPTION"),
        _ => None,
    }
}


/// HTTP method(s) from a config object's `method` value — a string (`'GET'`) or
/// an array (`['GET','POST']`). Upper-cased.
pub(super) fn config_route_methods(obj: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(value) = object_pair_value(obj, "method", src) else {
        return Vec::new();
    };
    match value.kind() {
        "string" => vec![unquote(&text(value, src)).to_ascii_uppercase()],
        "array" => {
            let mut out = Vec::new();
            let mut cursor = value.walk();
            for el in value.named_children(&mut cursor) {
                if el.kind() == "string" {
                    out.push(unquote(&text(el, src)).to_ascii_uppercase());
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `server.route({ method, path })` (hapi) / `fastify.route({ method, url })` —
/// config-object route registration. `path_key` is `"path"` (hapi) or `"url"` (fastify).
pub(super) fn emit_config_routes(
    call: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    source: RouteSource,
    path_key: &str,
) {
    let Some(arg0) = ts_positional_argument(call, 0) else {
        return;
    };
    if arg0.kind() != "object" {
        return;
    }
    let Some(path) = object_pair_value(arg0, path_key, src)
        .filter(|v| v.kind() == "string")
        .map(|v| unquote(&text(v, src)))
    else {
        return;
    };
    let methods = config_route_methods(arg0, src);
    let methods = if methods.is_empty() {
        vec!["ALL".to_string()]
    } else {
        methods
    };
    for m in methods {
        builder.emit_backend_route(call, source, &m, &path);
    }
}

/// Detect a backend HTTP route from a `call_expression` across Express, Fastify,
/// Koa (verb calls) and Fastify/Hapi (config-object `.route({...})`). Express is
/// unchanged; new frameworks are import-gated to disambiguate shared receiver
/// names (`app`/`router`).
pub(super) fn detect_call_route(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "member_expression" {
        return;
    }
    let (Some(obj), Some(prop_node)) = (
        func.child_by_field_name("object"),
        func.child_by_field_name("property"),
    ) else {
        return;
    };
    let object = text(obj, src);
    let prop = text(prop_node, src);

    // Config-object forms: `server.route({...})` (hapi), `fastify.route({...})`.
    if prop == "route" {
        if object == "server" && (builder.imports_pkg("@hapi/hapi") || builder.imports_pkg("hapi")) {
            emit_config_routes(node, src, builder, RouteSource::Hapi, "path");
            return;
        }
        if (object == "fastify" || object == "app") && builder.imports_pkg("fastify") {
            emit_config_routes(node, src, builder, RouteSource::Fastify, "url");
            return;
        }
    }

    // Verb forms: `<object>.<verb>(path, handler)`.
    let Some(source) = builder.route_framework_for(&object) else {
        return;
    };
    // Express keeps its original 5-verb set; the others get head/options/all too.
    let http_method = match source {
        RouteSource::Express => express_http_method(&prop),
        _ => any_http_verb(&prop),
    };
    let Some(http_method) = http_method else {
        return;
    };
    if let Some(path) = first_string_arg_in_call(node, src) {
        builder.emit_backend_route(node, source, http_method, &path);
    }
}

