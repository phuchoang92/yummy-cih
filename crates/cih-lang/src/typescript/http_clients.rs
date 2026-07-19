//! Outbound HTTP client detection — `fetch`/`axios`/Angular `HttpClient` calls
//! and same-repo `apiFetch`-style wrapper functions become consumer-side
//! `ContractSite`s (matched cross-repo against producer routes). tRPC and GraphQL
//! consumer operations are detected here too.

use cih_core::{
    file_id, ContractKind, ContractSite, HttpWrapperDef, NodeId, RouteSource, StringConstant,
    UrlPart,
};
use tree_sitter::Node as TsNode;

use crate::contracts_common::{
    http_verb_from_method, normalize_external_url, parts_have_nonlit, parts_start_with_abs_path,
    WrapperUrlPiece,
};

use super::builder::Builder;
use super::helpers::{
    literal_ts_string, object_pair_value, range_of, text, ts_positional_argument,
    unquote,
};

// ── Outbound HTTP contract sites (fetch / axios) ──────────────────────────────
//
// Tight recognizers to avoid false positives: bare `fetch(url[, {method}])`
// (default GET), `axios.<verb>(url, …)`, and `axios(url, {method})`. Instance
// clients (`this.http.get(...)`) are out of scope v1. URLs reuse the Phase B
// parts model: template-string substitutions become `Dynamic` parts and fold
// to `{*}` at resolve.

/// Fetch-like bare-identifier client whose method comes from the options object.
/// `fetch`/`axios`/`$fetch`/`ofetch` are distinctive enough to match unconditionally;
/// `got`/`ky` are import-gated (checked by the caller) as they collide with common names.
pub(super) fn fetch_like_identifier(callee: &str) -> bool {
    matches!(callee, "fetch" | "axios" | "$fetch" | "ofetch")
}

/// The receiver name of a member call for HttpClient detection: a bare identifier
/// (`http.get`) or a `this.<name>` member (`this.http.get`).
pub(super) fn httpclient_receiver_name(object_node: TsNode<'_>, src: &str) -> Option<String> {
    match object_node.kind() {
        "identifier" => Some(text(object_node, src)),
        "member_expression" => {
            let inner = object_node.child_by_field_name("object")?;
            if text(inner, src) == "this" {
                object_node.child_by_field_name("property").map(|p| text(p, src))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Resolve a `<receiver>.<verb>(...)` member call to `(http_method, base_url)` for
/// a recognized outbound client: axios (+ `axios.create()` instances), ky,
/// superagent, undici, and Angular/Nest HttpClient (`this.http`). `None` = not a client.
pub(super) fn resolve_client_call(
    node: TsNode<'_>,
    func: TsNode<'_>,
    src: &str,
    builder: &Builder,
) -> Option<(String, Option<String>)> {
    let object_node = func.child_by_field_name("object")?;
    let prop_node = func.child_by_field_name("property")?;
    let prop = text(prop_node, src);

    // Angular `HttpClient` / Nest `HttpService`: `(this.)http|httpClient|httpService.<verb>`,
    // import-gated to bound false positives on the generic verb names.
    if let Some(recv) = httpclient_receiver_name(object_node, src) {
        if matches!(recv.as_str(), "http" | "httpClient" | "httpService")
            && (builder.imports_pkg("@angular/common/http") || builder.imports_pkg("@nestjs/axios"))
        {
            if let Some(v) = http_verb_from_method(&prop) {
                return Some((v.to_string(), None));
            }
        }
    }

    // Identifier-receiver clients.
    if object_node.kind() == "identifier" {
        let obj = text(object_node, src);
        if obj == "axios" {
            return http_verb_from_method(&prop).map(|v| (v.to_string(), None));
        }
        if let Some(base) = builder.axios_instances.get(&obj) {
            return http_verb_from_method(&prop).map(|v| (v.to_string(), base.clone()));
        }
        if (obj == "ky" && builder.imports_pkg("ky"))
            || (obj == "superagent" && builder.imports_pkg("superagent"))
        {
            return http_verb_from_method(&prop).map(|v| (v.to_string(), None));
        }
        // undici: `undici.request(url, { method })` (method from options).
        if obj == "undici" && builder.imports_pkg("undici") && prop == "request" {
            return Some((call_options_method(node, src).unwrap_or_else(|| "GET".into()), None));
        }
    }
    None
}

/// Join a client baseURL with a call path (skips absolute URLs on the path side).
pub(super) fn join_client_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// True if `value` is an `axios.create(...)` call.
pub(super) fn is_axios_create(value: TsNode<'_>, src: &str) -> bool {
    if value.kind() != "call_expression" {
        return false;
    }
    let Some(func) = value.child_by_field_name("function") else {
        return false;
    };
    func.kind() == "member_expression"
        && func.child_by_field_name("object").map(|o| text(o, src)).as_deref() == Some("axios")
        && func
            .child_by_field_name("property")
            .map(|p| text(p, src))
            .as_deref()
            == Some("create")
}

/// Literal `baseURL` from an `axios.create({ baseURL })` config, if present.
pub(super) fn axios_create_base_url(value: TsNode<'_>, src: &str) -> Option<String> {
    let arg0 = ts_positional_argument(value, 0)?;
    if arg0.kind() != "object" {
        return None;
    }
    literal_ts_string(object_pair_value(arg0, "baseURL", src)?, src)
}

/// Pre-pass: record `const X = axios.create({ baseURL })` instances (name →
/// optional literal baseURL) so their `.get/.post/…` calls resolve as axios.
pub(super) fn collect_axios_instances(root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "variable_declarator" {
            if let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) {
                if name.kind() == "identifier" && is_axios_create(value, src) {
                    let base = axios_create_base_url(value, src);
                    builder.axios_instances.insert(text(name, src), base);
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
}

pub(super) fn try_emit_http_contract(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    // In-file module constant names — a `${ident}` naming one folds cross-file at
    // resolve time; other identifiers (params/locals) stay Dynamic.
    let consts: std::collections::HashSet<&str> = builder
        .string_constants
        .iter()
        .map(|c| c.const_name.as_str())
        .collect();
    let mut via_wrapper = None;
    // Literal baseURL prefix for `axios.create()` instance calls, applied below.
    let mut base_prefix: Option<String> = None;
    let http_method = match func.kind() {
        "identifier" => {
            let callee = text(func, src);
            if fetch_like_identifier(&callee)
                || (matches!(callee.as_str(), "got" | "ky") && builder.imports_pkg(&callee))
            {
                // Method comes from the second-arg options object, default GET.
                call_options_method(node, src).unwrap_or_else(|| "GET".into())
            } else {
                // Any other plain identifier MAY be a same-repo HTTP wrapper
                // (`apiFetch('/admin/x', { method: 'POST' }, token)`). Emit a
                // PROVISIONAL site only when arg 0 is URL-ish; the resolve phase
                // joins it against detected wrapper defs and drops non-matches.
                let Some(arg0) = ts_positional_argument(node, 0) else {
                    return;
                };
                if !ts_arg_is_url_ish(arg0, src, &consts) {
                    return;
                }
                via_wrapper = Some(callee);
                call_options_method(node, src).unwrap_or_else(|| "GET".into())
            }
        }
        "member_expression" => {
            if let Some((method, base)) = resolve_client_call(node, func, src, builder) {
                base_prefix = base;
                method
            } else {
                // Namespace-import alias receiver (`import * as api from
                // './apiClient'; api.apiFetch('/x')`) — bare identifiers matching
                // a known import alias only.
                let object_node = func.child_by_field_name("object");
                let Some(obj) = object_node.filter(|n| n.kind() == "identifier") else {
                    return;
                };
                let obj_text = text(obj, src);
                if !builder
                    .imports
                    .iter()
                    .any(|imp| !imp.is_static && imp.alias.as_deref() == Some(obj_text.as_str()))
                {
                    return;
                }
                let Some(prop) = func.child_by_field_name("property") else {
                    return;
                };
                let Some(arg0) = ts_positional_argument(node, 0) else {
                    return;
                };
                if !ts_arg_is_url_ish(arg0, src, &consts) {
                    return;
                }
                via_wrapper = Some(format!("{obj_text}.{}", text(prop, src)));
                call_options_method(node, src).unwrap_or_else(|| "GET".into())
            }
        }
        _ => return,
    };
    let Some(url_node) = ts_positional_argument(node, 0) else {
        return;
    };

    let (url_template, url_parts) = if via_wrapper.is_some() {
        // Wrapper calls ALWAYS carry parts — even plain literals — because the
        // resolve join must prepend the wrapper's base parts.
        let mut parts = Vec::new();
        fold_ts_url_expr(url_node, src, &mut parts, &consts);
        if parts.is_empty() {
            return;
        }
        (None, Some(parts))
    } else {
        let template = literal_ts_string(url_node, src).map(|url| {
            let full = match &base_prefix {
                Some(base) => join_client_url(base, &url),
                None => url,
            };
            normalize_external_url(&full)
        });
        let parts = if template.is_none() {
            ts_url_parts(url_node, src, &consts)
        } else {
            None
        };
        if template.is_none() && parts.is_none() {
            return;
        }
        (template, parts)
    };
    let in_callable = enclosing_fn
        .cloned()
        .unwrap_or_else(|| file_id(&builder.rel));
    builder.contract_sites.push(ContractSite {
        kind: ContractKind::HttpCall,
        url_template,
        topic: None,
        http_method: Some(http_method),
        messaging_framework: None,
        url_parts,
        via_wrapper,
        in_callable,
        range: range_of(node),
    });
}

/// tRPC procedure: `<proc>.query(resolver)` / `.mutation(...)` / `.subscription(...)`
/// in a file that imports `@trpc/server`. Import-gated + requires a function
/// argument to avoid react-query `.query` false positives.
pub(super) fn try_emit_trpc_contract(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    if !builder.imports_pkg("@trpc/server") {
        return;
    }
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "member_expression" {
        return;
    }
    let Some(prop) = func.child_by_field_name("property") else {
        return;
    };
    let op = match text(prop, src).as_str() {
        "query" => "QUERY",
        "mutation" => "MUTATION",
        "subscription" => "SUBSCRIPTION",
        _ => return,
    };
    let Some(arg0) = ts_positional_argument(node, 0) else {
        return;
    };
    if !matches!(
        arg0.kind(),
        "arrow_function" | "function" | "function_expression"
    ) {
        return;
    }
    // Procedure name = the enclosing router property key
    // (`getUser: t.procedure.query(...)`). Without one, skip — a nameless route
    // is useless.
    let Some(name) = trpc_procedure_name(node, src) else {
        return;
    };
    builder.emit_operation_route(node, RouteSource::Trpc, op, &name, enclosing_fn);
}

/// The router property key enclosing a tRPC procedure call
/// (`getUser: t.procedure.query(...)` → `"getUser"`). Walks up to the nearest
/// `pair`, stopping at function/statement boundaries.
pub(super) fn trpc_procedure_name(call: TsNode<'_>, src: &str) -> Option<String> {
    let mut n = call;
    while let Some(parent) = n.parent() {
        match parent.kind() {
            "pair" => {
                return parent.child_by_field_name("key").map(|k| unquote(&text(k, src)));
            }
            "statement_block" | "program" | "function_declaration" | "arrow_function"
            | "method_definition" => return None,
            _ => n = parent,
        }
    }
    None
}

/// tRPC CONSUMER call: `trpc.<...>.<proc>.useQuery|query|useMutation|mutate|
/// useSubscription|subscribe(...)`. Import-gated on a tRPC *client* package;
/// requires a member-chain receiver (so React-Query's bare `useQuery(...)` and
/// the producer `t.procedure.query(fn)` are excluded). Emits a consumer contract
/// keyed by (method, procedure name) that the matcher links to the producer Route.
pub(super) fn try_emit_trpc_consumer(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    if !(builder.imports_pkg("@trpc/react-query")
        || builder.imports_pkg("@trpc/client")
        || builder.imports_pkg("@trpc/next"))
    {
        return;
    }
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "member_expression" {
        return;
    }
    let Some(prop) = func.child_by_field_name("property") else {
        return;
    };
    let arg0_is_fn = ts_positional_argument(node, 0)
        .is_some_and(|a| matches!(a.kind(), "arrow_function" | "function" | "function_expression"));
    let method = match text(prop, src).as_str() {
        "useQuery" => "QUERY",
        "useMutation" | "mutate" => "MUTATION",
        "useSubscription" | "subscribe" => "SUBSCRIPTION",
        // `.query(input)` on the client — a `.query(fn)` is the producer.
        "query" if !arg0_is_fn => "QUERY",
        _ => return,
    };
    // Procedure name = the last property of the receiver chain (`trpc.user.getUser`).
    let Some(recv) = func.child_by_field_name("object") else {
        return;
    };
    if recv.kind() != "member_expression" {
        return;
    }
    let Some(name_node) = recv.child_by_field_name("property") else {
        return;
    };
    let in_callable = enclosing_fn
        .cloned()
        .unwrap_or_else(|| file_id(&builder.rel));
    builder.emit_operation_call(node, method, &text(name_node, src), in_callable);
}

/// GraphQL CONSUMER: a `gql`/`graphql` tagged template holding an operation.
/// Emits a consumer contract keyed by (operation type, first root field).
pub(super) fn try_emit_graphql_consumer(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "identifier" || !matches!(text(func, src).as_str(), "gql" | "graphql") {
        return;
    }
    let Some(tmpl) = node.child_by_field_name("arguments") else {
        return;
    };
    if tmpl.kind() != "template_string" {
        return;
    }
    let mut body = String::new();
    let mut cursor = tmpl.walk();
    for child in tmpl.named_children(&mut cursor) {
        if child.kind() == "string_fragment" {
            body.push_str(child.utf8_text(src.as_bytes()).unwrap_or_default());
        }
    }
    let Some((method, field)) = graphql_root_op(&body) else {
        return;
    };
    let in_callable = enclosing_fn
        .cloned()
        .unwrap_or_else(|| file_id(&builder.rel));
    builder.emit_operation_call(node, method, &field, in_callable);
}

/// From a GraphQL document body, the operation type + first root field
/// (`query GetMe { me { id } }` → `("QUERY", "me")`; anonymous `{ me }` → query).
pub(super) fn graphql_root_op(body: &str) -> Option<(&'static str, String)> {
    let body = body.trim_start();
    let boundary = |r: &str| r.starts_with(|c: char| c.is_whitespace() || c == '{' || c == '(');
    let (method, rest) = if let Some(r) = body.strip_prefix("query").filter(|r| boundary(r)) {
        ("QUERY", r)
    } else if let Some(r) = body.strip_prefix("mutation").filter(|r| boundary(r)) {
        ("MUTATION", r)
    } else if let Some(r) = body.strip_prefix("subscription").filter(|r| boundary(r)) {
        ("SUBSCRIPTION", r)
    } else if body.starts_with('{') {
        ("QUERY", body)
    } else {
        return None;
    };
    let after = &rest[rest.find('{')? + 1..];
    let field: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!field.is_empty()).then_some((method, field))
}

/// URL-ish gate for provisional wrapper calls: a string starting with `/`, a
/// template whose first fragment starts with `/`, or a concat whose folded
/// first part is such a Lit. Keeps `t('common.title')` / `helper(id)` out.
pub(super) fn ts_arg_is_url_ish(node: TsNode<'_>, src: &str, consts: &std::collections::HashSet<&str>) -> bool {
    let mut parts = Vec::new();
    fold_ts_url_expr(node, src, &mut parts, consts);
    parts_start_with_abs_path(&parts)
}


/// `method: 'POST'` from a call's second-argument options object literal.
pub(super) fn call_options_method(call: TsNode<'_>, src: &str) -> Option<String> {
    let options = ts_positional_argument(call, 1)?;
    if options.kind() != "object" {
        return None;
    }
    let mut cursor = options.walk();
    for entry in options.named_children(&mut cursor) {
        if entry.kind() != "pair" {
            continue;
        }
        let key = entry
            .child_by_field_name("key")
            .map(|key| unquote(&text(key, src)))
            .unwrap_or_default();
        if key != "method" {
            continue;
        }
        let value = entry.child_by_field_name("value")?;
        if value.kind() == "string" {
            return Some(unquote(&text(value, src)).to_ascii_uppercase());
        }
        return None;
    }
    None
}


/// Phase B parts for a non-literal URL argument: template-string fragments →
/// `Lit`, a `${IDENT}` substitution → `ConstRef` (resolved cross-file via
/// module constants and the gated unique-name fallback), any other `${…}` →
/// `Dynamic`, `+`-concat folds recursively. Unresolved refs degrade to `{*}`
/// — never a wrong match.
pub(super) fn ts_url_parts(
    node: TsNode<'_>,
    src: &str,
    consts: &std::collections::HashSet<&str>,
) -> Option<Vec<UrlPart>> {
    let mut parts = Vec::new();
    fold_ts_url_expr(node, src, &mut parts, consts);
    parts_have_nonlit(&parts).then_some(parts)
}

pub(super) fn fold_ts_url_expr(
    node: TsNode<'_>,
    src: &str,
    out: &mut Vec<UrlPart>,
    consts: &std::collections::HashSet<&str>,
) {
    match node.kind() {
        "string" => out.push(UrlPart::Lit(unquote(&text(node, src)))),
        "template_string" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_fragment" | "escape_sequence" => out.push(UrlPart::Lit(
                        child.utf8_text(src.as_bytes()).unwrap_or_default().to_string(),
                    )),
                    // `${IDENT}` → ConstRef when IDENT is a SCREAMING_SNAKE name
                    // (imported/external constants) OR a known in-file module
                    // constant (`${apiBase}`). Params/locals (`${id}`, `${userId}`)
                    // and property chains (`${cfg.base}`) stay Dynamic so they
                    // never feed the cross-file unique-name fallback.
                    "template_substitution" => match child.named_child(0) {
                        Some(inner) if inner.kind() == "identifier" => {
                            let name = text(inner, src);
                            if crate::contracts_common::is_screaming_snake(&name)
                                || consts.contains(name.as_str())
                            {
                                out.push(UrlPart::ConstRef(name));
                            } else {
                                out.push(UrlPart::Dynamic);
                            }
                        }
                        _ => out.push(UrlPart::Dynamic),
                    },
                    _ => {}
                }
            }
        }
        "binary_expression" => {
            let op = node.child_by_field_name("operator").map(|op| text(op, src));
            if op.as_deref() != Some("+") {
                out.push(UrlPart::Dynamic);
                return;
            }
            match node.child_by_field_name("left") {
                Some(left) => fold_ts_url_expr(left, src, out, consts),
                None => out.push(UrlPart::Dynamic),
            }
            match node.child_by_field_name("right") {
                Some(right) => fold_ts_url_expr(right, src, out, consts),
                None => out.push(UrlPart::Dynamic),
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => fold_ts_url_expr(inner, src, out, consts),
            None => out.push(UrlPart::Dynamic),
        },
        "identifier" | "member_expression" => out.push(UrlPart::ConstRef(text(node, src))),
        _ => out.push(UrlPart::Dynamic),
    }
}

/// Module-level `const X = <init>` → `StringConstant` when the initializer is
/// a plain string literal, or an env-override with a literal default
/// (`import.meta.env.X ?? '/api/v1'`, `x || '/api'`) — the default becomes the
/// value with `env_default: true`. Anything else emits nothing (the resolver
/// then degrades references to `{*}`, never a guess). `let`/`var` are skipped:
/// only `const` is reliably constant.
pub(super) fn collect_module_string_constants(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let is_const = node
        .child_by_field_name("kind")
        .or_else(|| node.child(0))
        .map(|kind| text(kind, src) == "const")
        .unwrap_or(false);
    if !is_const {
        return;
    }
    let mut cursor = node.walk();
    for declarator in node.named_children(&mut cursor) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        if name_node.kind() != "identifier" {
            continue;
        }
        let Some(value_node) = declarator.child_by_field_name("value") else {
            continue;
        };
        let (value, env_default) = match value_node.kind() {
            "string" => (unquote(&text(value_node, src)), false),
            "binary_expression" => {
                let op = value_node
                    .child_by_field_name("operator")
                    .map(|op| text(op, src));
                let right = value_node.child_by_field_name("right");
                match (op.as_deref(), right) {
                    (Some("??") | Some("||"), Some(right)) if right.kind() == "string" => {
                        (unquote(&text(right, src)), true)
                    }
                    _ => continue,
                }
            }
            _ => continue,
        };
        builder.string_constants.push(StringConstant {
            const_name: text(name_node, src),
            owner_fqcn: builder.module.clone(),
            value,
            dynamic: false,
            env_default,
            range: range_of(declarator),
        });
    }
}

// ── HTTP wrapper detection (apiFetch pattern) ────────────────────────────────

/// Detect a same-repo HTTP wrapper: a module-scope function whose FIRST param
/// is a plain identifier and whose body calls fetch/axios with a URL that is
/// `<Lit/ConstRef prefix…><param>` (param LAST) — directly or via one level of
/// `const url = <expr>` same-body indirection. Anything fancier bails: a
/// missed wrapper degrades coverage, a wrong one would fabricate endpoints.
pub(super) fn try_collect_http_wrapper(name: &str, fn_node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(param_name) = first_param_identifier(fn_node, src) else {
        return;
    };
    let Some(body) = fn_node.child_by_field_name("body") else {
        return;
    };
    let Some(http_call) = find_inner_http_call(body, src) else {
        return;
    };
    let Some(mut url_expr) = ts_positional_argument(http_call, 0) else {
        return;
    };
    // One-level indirection: `const url = <expr>; … fetch(url, …)`.
    if url_expr.kind() == "identifier" {
        let local = text(url_expr, src);
        if local == param_name {
            // fetch(param) directly: empty prefix — a pure pass-through.
            builder.http_wrappers.push(HttpWrapperDef {
                name: name.to_string(),
                module: builder.module.clone(),
                prefix_parts: Vec::new(),
                options_arg_index: 1,
                fixed_method: None,
                range: range_of(fn_node),
            });
            return;
        }
        match find_unique_const_initializer(body, &local, src) {
            Some(value) => url_expr = value,
            None => return,
        }
    }
    let mut pieces = Vec::new();
    fold_wrapper_url_expr(url_expr, src, &param_name, &mut pieces);
    // Param must be the FINAL piece, appear exactly once, and everything
    // before it must be Lit/ConstRef.
    let Some(WrapperUrlPiece::Param) = pieces.last() else {
        return;
    };
    let prefix: Vec<UrlPart> = pieces[..pieces.len() - 1]
        .iter()
        .map(|piece| match piece {
            WrapperUrlPiece::Part(part) => Some(part.clone()),
            WrapperUrlPiece::Param => None,
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    if pieces.len() > 1 && prefix.is_empty() {
        return; // a second Param (or nothing collectible) — bail
    }
    if prefix
        .iter()
        .any(|part| matches!(part, UrlPart::Dynamic))
    {
        return;
    }
    builder.http_wrappers.push(HttpWrapperDef {
        name: name.to_string(),
        module: builder.module.clone(),
        prefix_parts: prefix,
        options_arg_index: 1,
                fixed_method: None,
        range: range_of(fn_node),
    });
}

/// The function's first parameter when it is a plain identifier pattern
/// (typed `endpoint: string` included); destructuring → None.
pub(super) fn first_param_identifier(fn_node: TsNode<'_>, src: &str) -> Option<String> {
    let params = fn_node
        .child_by_field_name("parameters")
        .or_else(|| fn_node.child_by_field_name("parameter"))?;
    if params.kind() == "identifier" {
        // bare single-param arrow: `endpoint => …`
        return Some(text(params, src));
    }
    let mut cursor = params.walk();
    let first = params
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "required_parameter" | "optional_parameter"))?;
    let pattern = first.child_by_field_name("pattern")?;
    (pattern.kind() == "identifier").then(|| text(pattern, src))
}

/// First fetch/axios call inside `body`, NOT descending into nested function
/// definitions (a closure must not make its enclosing function a wrapper).
pub(super) fn find_inner_http_call<'a>(body: TsNode<'a>, src: &str) -> Option<TsNode<'a>> {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "function_declaration" | "arrow_function" | "function_expression" | "method_definition"
        ) {
            continue;
        }
        if child.kind() == "call_expression" {
            if let Some(func) = child.child_by_field_name("function") {
                let is_http = match func.kind() {
                    "identifier" => {
                        let callee = text(func, src);
                        callee == "fetch" || callee == "axios"
                    }
                    "member_expression" => {
                        func.child_by_field_name("object")
                            .map(|obj| text(obj, src) == "axios")
                            .unwrap_or(false)
                    }
                    _ => false,
                };
                if is_http {
                    return Some(child);
                }
            }
        }
        if let Some(found) = find_inner_http_call(child, src) {
            return Some(found);
        }
    }
    None
}

/// The unique same-body `const <local> = <expr>` initializer, or None when
/// absent or ambiguous (shadowing across branches → refuse to guess).
pub(super) fn find_unique_const_initializer<'a>(
    body: TsNode<'a>,
    local: &str,
    src: &str,
) -> Option<TsNode<'a>> {
    let mut found: Option<TsNode<'a>> = None;
    collect_const_initializers(body, local, src, &mut found, &mut 0);
    found
}

pub(super) fn collect_const_initializers<'a>(
    node: TsNode<'a>,
    local: &str,
    src: &str,
    found: &mut Option<TsNode<'a>>,
    count: &mut u32,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "function_declaration" | "arrow_function" | "function_expression" | "method_definition"
        ) {
            continue;
        }
        if child.kind() == "lexical_declaration" {
            let mut dcursor = child.walk();
            for declarator in child.named_children(&mut dcursor) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                let name = declarator
                    .child_by_field_name("name")
                    .map(|n| text(n, src))
                    .unwrap_or_default();
                if name == local {
                    if let Some(value) = declarator.child_by_field_name("value") {
                        *count += 1;
                        if *count > 1 {
                            *found = None;
                            return;
                        }
                        *found = Some(value);
                    }
                }
            }
        }
        collect_const_initializers(child, local, src, found, count);
        if *count > 1 {
            return;
        }
    }
}

/// Fold a wrapper's URL expression like [`fold_ts_url_expr`], except any
/// identifier equal to the pass-through param becomes [`WrapperUrlPiece::Param`].
pub(super) fn fold_wrapper_url_expr(
    node: TsNode<'_>,
    src: &str,
    param: &str,
    out: &mut Vec<WrapperUrlPiece>,
) {
    match node.kind() {
        "identifier" if text(node, src) == param => out.push(WrapperUrlPiece::Param),
        "template_string" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_fragment" | "escape_sequence" => {
                        out.push(WrapperUrlPiece::Part(UrlPart::Lit(
                            child
                                .utf8_text(src.as_bytes())
                                .unwrap_or_default()
                                .to_string(),
                        )))
                    }
                    "template_substitution" => match child.named_child(0) {
                        Some(inner) if inner.kind() == "identifier" && text(inner, src) == param => {
                            out.push(WrapperUrlPiece::Param)
                        }
                        Some(inner)
                            if inner.kind() == "identifier"
                                && crate::contracts_common::is_screaming_snake(&text(
                                    inner, src,
                                )) =>
                        {
                            out.push(WrapperUrlPiece::Part(UrlPart::ConstRef(text(inner, src))))
                        }
                        _ => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
                    },
                    _ => {}
                }
            }
        }
        "binary_expression" => {
            let op = node.child_by_field_name("operator").map(|op| text(op, src));
            if op.as_deref() != Some("+") {
                out.push(WrapperUrlPiece::Part(UrlPart::Dynamic));
                return;
            }
            for field in ["left", "right"] {
                match node.child_by_field_name(field) {
                    Some(side) => fold_wrapper_url_expr(side, src, param, out),
                    None => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
                }
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => fold_wrapper_url_expr(inner, src, param, out),
            None => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
        },
        "string" => out.push(WrapperUrlPiece::Part(UrlPart::Lit(unquote(&text(
            node, src,
        ))))),
        "identifier" if crate::contracts_common::is_screaming_snake(&text(node, src)) => {
            out.push(WrapperUrlPiece::Part(UrlPart::ConstRef(text(node, src))))
        }
        _ => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
    }
}

