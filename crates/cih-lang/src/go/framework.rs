//! Go framework detection: HTTP routes (net/http, gin, echo, chi,
//! gorilla/mux) and outbound HTTP calls (`http.Get`, `http.NewRequest`, …).
//!
//! Go has no annotations, so detection is **import-gated per library** and
//! then shape-gated (verb-named method whose first argument is a route-shaped
//! string). New logic with no cross-language precedent to port — Express emits
//! Route nodes with no handler edge; Go emits `HandlesRoute` only when the
//! handler argument is a plain identifier matching a same-file function.
//! Route ids use the Express-style `Route:go:{METHOD}:{path}` convention
//! (documented in docs/ARCHITECTURE.md).

use std::collections::HashMap;

use cih_core::{
    ContractKind, ContractSite, Edge, EdgeKind, Node, NodeId, NodeKind, RawImport, RouteSource,
    UrlPart,
};
use tree_sitter::Node as TsNode;

use super::parse::{range_of, text, unquote};
use crate::contracts_common::normalize_external_url;

/// Which HTTP libraries the file imports — gates every detection below.
#[derive(Default)]
pub(super) struct GoFrameworkCtx {
    net_http: bool,
    gin: bool,
    echo: bool,
    chi: bool,
    gorilla_mux: bool,
}

impl GoFrameworkCtx {
    pub(super) fn from_imports(imports: &[RawImport]) -> Self {
        let mut ctx = Self::default();
        for import in imports {
            let path = import.raw.as_str();
            if path == "net/http" {
                ctx.net_http = true;
            } else if path.starts_with("github.com/gin-gonic/gin") {
                ctx.gin = true;
            } else if path.starts_with("github.com/labstack/echo") {
                ctx.echo = true;
            } else if path.starts_with("github.com/go-chi/chi") {
                ctx.chi = true;
            } else if path.starts_with("github.com/gorilla/mux") {
                ctx.gorilla_mux = true;
            }
        }
        ctx
    }

    pub(super) fn any(&self) -> bool {
        self.net_http || self.gin || self.echo || self.chi || self.gorilla_mux
    }
}

const HTTP_VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

/// Walk one function/method body for route registrations and outbound calls.
#[allow(clippy::too_many_arguments)] // parser-pass plumbing, mirrors the other walkers
pub(super) fn collect_contracts(
    fn_node: TsNode<'_>,
    src: &str,
    ctx: &GoFrameworkCtx,
    in_callable: &NodeId,
    file_fn_ids: &HashMap<String, NodeId>,
    rel: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    contract_sites: &mut Vec<ContractSite>,
) {
    let mut stack = vec![fn_node];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            try_emit_route(node, src, ctx, file_fn_ids, rel, nodes, edges);
            try_emit_outbound(node, src, ctx, in_callable, contract_sites);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

// ── Routes ───────────────────────────────────────────────────────────────────

/// `(method, path)` when a registration method + first-arg pattern is
/// route-shaped. Go 1.22 `"GET /orders/{id}"` patterns split into their verb;
/// plain `HandleFunc("/path", …)` registrations are method-`ANY`.
fn route_registration(
    method_name: &str,
    pattern: &str,
    ctx: &GoFrameworkCtx,
) -> Option<(String, String)> {
    // gin / echo: r.GET("/path", h)
    if (ctx.gin || ctx.echo) && HTTP_VERBS.contains(&method_name) && pattern.starts_with('/') {
        return Some((method_name.to_string(), pattern.to_string()));
    }
    // chi: r.Get("/path", h)
    if ctx.chi
        && HTTP_VERBS
            .iter()
            .any(|verb| is_capitalized(method_name, verb))
        && pattern.starts_with('/')
    {
        return Some((method_name.to_ascii_uppercase(), pattern.to_string()));
    }
    // net/http / gorilla: mux.HandleFunc("/path", h) / http.Handle("GET /path", h)
    if (ctx.net_http || ctx.gorilla_mux) && matches!(method_name, "HandleFunc" | "Handle") {
        if let Some((verb, path)) = pattern.split_once(' ') {
            // Go 1.22 method pattern.
            if HTTP_VERBS.contains(&verb) && path.starts_with('/') {
                return Some((verb.to_string(), path.to_string()));
            }
        }
        if pattern.starts_with('/') {
            return Some(("ANY".to_string(), pattern.to_string()));
        }
    }
    None
}

fn is_capitalized(name: &str, verb: &str) -> bool {
    name.len() == verb.len()
        && name.chars().next() == verb.chars().next()
        && name[1..].eq_ignore_ascii_case(&verb[1..])
        && name[1..].chars().all(|c| c.is_ascii_lowercase())
}

fn try_emit_route(
    call: TsNode<'_>,
    src: &str,
    ctx: &GoFrameworkCtx,
    file_fn_ids: &HashMap<String, NodeId>,
    rel: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    let Some(func) = call.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "selector_expression" {
        return;
    }
    let Some(method_name) = func.child_by_field_name("field").map(|n| text(n, src)) else {
        return;
    };
    let Some(pattern) = positional_argument(call, 0)
        .filter(|arg| arg.kind() == "interpreted_string_literal")
        .map(|arg| unquote(text(arg, src)))
    else {
        return;
    };
    let Some((http_method, path)) = route_registration(method_name, &pattern, ctx) else {
        return;
    };

    // Handler resolution: only a plain identifier naming a same-file function.
    let handler = positional_argument(call, 1)
        .filter(|arg| arg.kind() == "identifier")
        .and_then(|arg| file_fn_ids.get(text(arg, src)));

    let route_id = NodeId::new(format!("Route:go:{http_method}:{path}"));
    let name = format!("{http_method} {path}");
    nodes.push(Node {
        id: route_id.clone(),
        kind: NodeKind::Route,
        name: name.clone(),
        qualified_name: Some(name),
        file: rel.to_string(),
        range: range_of(call),
        props: Some(serde_json::json!({
            "httpMethod": http_method,
            "path": path,
            "route_annotations": [],
            "source": RouteSource::Go,
            "handler": handler.map(|id| id.as_str()),
        })),
    });
    if let Some(handler_id) = handler {
        edges.push(Edge {
            src: handler_id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("go-{}", http_method.to_ascii_lowercase()),
            props: None,
        });
    }
}

// ── Outbound HTTP calls ──────────────────────────────────────────────────────

fn try_emit_outbound(
    call: TsNode<'_>,
    src: &str,
    ctx: &GoFrameworkCtx,
    in_callable: &NodeId,
    contract_sites: &mut Vec<ContractSite>,
) {
    if !ctx.net_http {
        return;
    }
    let Some(func) = call.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "selector_expression" {
        return;
    }
    let operand = func
        .child_by_field_name("operand")
        .map(|n| text(n, src))
        .unwrap_or_default();
    if operand != "http" {
        return;
    }
    let field = func
        .child_by_field_name("field")
        .map(|n| text(n, src))
        .unwrap_or_default();

    // `client.Do(req)` is deliberately skipped: the URL lives on the request.
    let (http_method, url_index) = match field {
        "Get" => ("GET".to_string(), 0),
        "Head" => ("HEAD".to_string(), 0),
        "Post" | "PostForm" => ("POST".to_string(), 0),
        "NewRequest" | "NewRequestWithContext" => {
            let method_index = if field == "NewRequest" { 0 } else { 1 };
            let Some(method) = positional_argument(call, method_index)
                .filter(|arg| arg.kind() == "interpreted_string_literal")
                .map(|arg| unquote(text(arg, src)).to_ascii_uppercase())
            else {
                return;
            };
            (method, method_index + 1)
        }
        _ => return,
    };
    let Some(url_node) = positional_argument(call, url_index) else {
        return;
    };

    let url_template = literal_go_string(url_node, src).map(|url| normalize_external_url(&url));
    let url_parts = if url_template.is_none() {
        go_url_parts(url_node, src)
    } else {
        None
    };
    if url_template.is_none() && url_parts.is_none() {
        return;
    }
    contract_sites.push(ContractSite {
        kind: ContractKind::HttpCall,
        url_template,
        topic: None,
        http_method: Some(http_method),
        messaging_framework: None,
        url_parts,
        in_callable: in_callable.clone(),
        range: range_of(call),
    });
}

fn positional_argument<'a>(call: TsNode<'a>, n: usize) -> Option<TsNode<'a>> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut index = 0;
    for child in args.named_children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        if index == n {
            return Some(child);
        }
        index += 1;
    }
    None
}

fn literal_go_string(node: TsNode<'_>, src: &str) -> Option<String> {
    matches!(node.kind(), "interpreted_string_literal" | "raw_string_literal")
        .then(|| unquote(text(node, src)))
}

/// Phase B parts for a non-literal URL argument: `+`-concat folds
/// (identifier / selector → `ConstRef`), `fmt.Sprintf` format strings split on
/// their `%` directives (`Lit` chunks, `Dynamic` per directive), anything else
/// is `Dynamic`. Unresolved parts degrade to `{*}` at resolve.
fn go_url_parts(node: TsNode<'_>, src: &str) -> Option<Vec<UrlPart>> {
    let mut parts = Vec::new();
    fold_go_url_expr(node, src, &mut parts);
    parts
        .iter()
        .any(|part| !matches!(part, UrlPart::Lit(_)))
        .then_some(parts)
}

fn fold_go_url_expr(node: TsNode<'_>, src: &str, out: &mut Vec<UrlPart>) {
    match node.kind() {
        "interpreted_string_literal" | "raw_string_literal" => {
            out.push(UrlPart::Lit(unquote(text(node, src))));
        }
        "binary_expression" => {
            let op = node.child_by_field_name("operator").map(|op| text(op, src));
            if op != Some("+") {
                out.push(UrlPart::Dynamic);
                return;
            }
            match node.child_by_field_name("left") {
                Some(left) => fold_go_url_expr(left, src, out),
                None => out.push(UrlPart::Dynamic),
            }
            match node.child_by_field_name("right") {
                Some(right) => fold_go_url_expr(right, src, out),
                None => out.push(UrlPart::Dynamic),
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => fold_go_url_expr(inner, src, out),
            None => out.push(UrlPart::Dynamic),
        },
        "identifier" => out.push(UrlPart::ConstRef(text(node, src).to_string())),
        "selector_expression" => out.push(UrlPart::ConstRef(text(node, src).to_string())),
        "call_expression" => {
            if is_sprintf(node, src) {
                if let Some(format) = positional_argument(node, 0)
                    .filter(|arg| arg.kind() == "interpreted_string_literal")
                    .map(|arg| unquote(text(arg, src)))
                {
                    fold_sprintf_format(&format, out);
                    return;
                }
            }
            out.push(UrlPart::Dynamic);
        }
        _ => out.push(UrlPart::Dynamic),
    }
}

fn is_sprintf(call: TsNode<'_>, src: &str) -> bool {
    call.child_by_field_name("function")
        .filter(|func| func.kind() == "selector_expression")
        .is_some_and(|func| {
            func.child_by_field_name("operand")
                .map(|n| text(n, src) == "fmt")
                .unwrap_or(false)
                && func
                    .child_by_field_name("field")
                    .map(|n| text(n, src) == "Sprintf")
                    .unwrap_or(false)
        })
}

/// Split a Sprintf format string on its `%` directives: literal chunks become
/// `Lit`, each directive becomes `Dynamic` (`%%` is a literal percent).
fn fold_sprintf_format(format: &str, out: &mut Vec<UrlPart>) {
    let mut lit = String::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            lit.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => lit.push('%'),
            Some(_) => {
                if !lit.is_empty() {
                    out.push(UrlPart::Lit(std::mem::take(&mut lit)));
                }
                out.push(UrlPart::Dynamic);
            }
            None => lit.push('%'),
        }
    }
    if !lit.is_empty() {
        out.push(UrlPart::Lit(lit));
    }
}
