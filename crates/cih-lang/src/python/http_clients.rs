//! Outbound HTTP client detection (Python) — `requests`/`httpx` module-receiver
//! calls and same-repo wrapper `def`s become consumer-side `ContractSite`s,
//! matched cross-repo against producer routes.

use cih_core::{
    file_id, ContractKind, ContractSite, HttpWrapperDef, NodeId, RawImport,
    StringConstant, UrlPart,
};
use tree_sitter::Node as TsNode;

use crate::contracts_common::normalize_external_url;

use super::builder::Builder;
use super::helpers::*;

// ── HTTP wrapper detection (python analog of the TS apiFetch pattern) ────────

/// One piece of a candidate wrapper's URL expression: a regular part, or the
/// pass-through parameter slot.
pub(super) enum WrapperUrlPiece {
    Part(UrlPart),
    Param,
}

/// Detect a same-repo HTTP wrapper: a module-scope `def` whose FIRST param is
/// a plain identifier and whose body calls `requests.<verb>` / `httpx.<verb>`
/// / `requests.request("VERB", …)` with a URL of `<Lit/ConstRef prefix><param>`
/// (param LAST) — directly or via one `url = <expr>` same-body assignment.
/// Python wrappers hard-code their verb, recorded as `fixed_method`. Anything
/// fancier bails: a missed wrapper degrades coverage, a wrong one would
/// fabricate endpoints.
pub(super) fn try_collect_py_http_wrapper(name: &str, fn_node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(param_name) = first_py_param_identifier(fn_node, src) else {
        return;
    };
    let Some(body) = fn_node.child_by_field_name("body") else {
        return;
    };
    let Some(http_call) = find_inner_py_http_call(body, src) else {
        return;
    };
    let Some(func) = http_call.child_by_field_name("function") else {
        return;
    };
    let attr = func
        .child_by_field_name("attribute")
        .map(|n| text(n, src))
        .unwrap_or_default();
    let (method, url_arg_index) = if attr == "request" {
        // requests.request("POST", url) — literal verb only; a method-param
        // pass-through (`def call(method, path)`) bails.
        let Some(verb) = positional_argument(http_call, 0)
            .filter(|arg| arg.kind() == "string")
            .and_then(|arg| literal_py_string(arg, src))
        else {
            return;
        };
        (verb.to_ascii_uppercase(), 1)
    } else {
        let Some(verb) = python_http_verb(&attr) else {
            return;
        };
        (verb.to_string(), 0)
    };
    let Some(mut url_expr) = positional_argument(http_call, url_arg_index) else {
        return;
    };
    // One-level indirection: `url = f"{API_BASE}{path}"` then verb(url).
    if url_expr.kind() == "identifier" {
        let local = text(url_expr, src);
        if local == param_name {
            // Pure pass-through: verb(param) — empty prefix.
            builder.http_wrappers.push(HttpWrapperDef {
                name: name.to_string(),
                module: builder.module.clone(),
                prefix_parts: Vec::new(),
                options_arg_index: 1,
                fixed_method: Some(method),
                range: range_of(fn_node),
            });
            return;
        }
        match find_unique_py_assignment(body, &local, src) {
            Some(value) => url_expr = value,
            None => return,
        }
    }
    let mut pieces = Vec::new();
    fold_wrapper_py_url_expr(url_expr, src, &param_name, &mut pieces);
    let Some(WrapperUrlPiece::Param) = pieces.last() else {
        return;
    };
    let Some(prefix) = pieces[..pieces.len() - 1]
        .iter()
        .map(|piece| match piece {
            WrapperUrlPiece::Part(part) => Some(part.clone()),
            WrapperUrlPiece::Param => None, // a second Param — bail
        })
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    if prefix.iter().any(|part| matches!(part, UrlPart::Dynamic)) {
        return;
    }
    builder.http_wrappers.push(HttpWrapperDef {
        name: name.to_string(),
        module: builder.module.clone(),
        prefix_parts: prefix,
        options_arg_index: 1,
        fixed_method: Some(method),
        range: range_of(fn_node),
    });
}

/// First parameter when it is a plain identifier (typed params included);
/// `self`/`cls` and destructuring → None. Deliberately NOT `parameter_count`,
/// which unconditionally subtracts one.
pub(super) fn first_py_param_identifier(fn_node: TsNode<'_>, src: &str) -> Option<String> {
    let params = fn_node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    let first = params.named_children(&mut cursor).next()?;
    let name = match first.kind() {
        "identifier" => text(first, src),
        "typed_parameter" => {
            let mut inner_cursor = first.walk();
            let inner = first
                .named_children(&mut inner_cursor)
                .find(|child| child.kind() == "identifier")?;
            text(inner, src)
        }
        _ => return None,
    };
    (name != "self" && name != "cls").then_some(name)
}

/// First `requests.*`/`httpx.*` call inside `body`, NOT descending into nested
/// function/class definitions or lambdas.
pub(super) fn find_inner_py_http_call<'a>(body: TsNode<'a>, src: &str) -> Option<TsNode<'a>> {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "function_definition" | "decorated_definition" | "lambda" | "class_definition"
        ) {
            continue;
        }
        if child.kind() == "call" {
            if let Some(func) = child.child_by_field_name("function") {
                if func.kind() == "attribute" {
                    let object = func
                        .child_by_field_name("object")
                        .filter(|obj| obj.kind() == "identifier")
                        .map(|obj| text(obj, src))
                        .unwrap_or_default();
                    let attr = func
                        .child_by_field_name("attribute")
                        .map(|n| text(n, src))
                        .unwrap_or_default();
                    if (object == "requests" || object == "httpx")
                        && (python_http_verb(&attr).is_some() || attr == "request")
                    {
                        return Some(child);
                    }
                }
            }
        }
        if let Some(found) = find_inner_py_http_call(child, src) {
            return Some(found);
        }
    }
    None
}

/// The unique same-body `local = <expr>` assignment, or None when absent or
/// ambiguous (reassignment across branches → refuse to guess).
pub(super) fn find_unique_py_assignment<'a>(body: TsNode<'a>, local: &str, src: &str) -> Option<TsNode<'a>> {
    let mut found: Option<TsNode<'a>> = None;
    let mut count = 0u32;
    collect_py_assignments(body, local, src, &mut found, &mut count);
    (count == 1).then_some(found).flatten()
}

pub(super) fn collect_py_assignments<'a>(
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
            "function_definition" | "decorated_definition" | "lambda" | "class_definition"
        ) {
            continue;
        }
        if child.kind() == "assignment" {
            let lhs = child
                .child_by_field_name("left")
                .filter(|left| left.kind() == "identifier")
                .map(|left| text(left, src));
            if lhs.as_deref() == Some(local) {
                if let Some(value) = child.child_by_field_name("right") {
                    *count += 1;
                    *found = Some(value);
                }
            }
        }
        collect_py_assignments(child, local, src, found, count);
        if *count > 1 {
            return;
        }
    }
}

/// Fold a wrapper's URL expression like [`fold_py_url_expr`], except any
/// reference to the pass-through param — f-string `{param}` interpolation or
/// bare `param` identifier — becomes [`WrapperUrlPiece::Param`] (checked
/// BEFORE the SCREAMING_SNAKE gate / ungated ConstRef arm respectively).
pub(super) fn fold_wrapper_py_url_expr(
    node: TsNode<'_>,
    src: &str,
    param: &str,
    out: &mut Vec<WrapperUrlPiece>,
) {
    match node.kind() {
        "identifier" if text(node, src) == param => out.push(WrapperUrlPiece::Param),
        "string" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_content" | "escape_sequence" => {
                        out.push(WrapperUrlPiece::Part(UrlPart::Lit(
                            child
                                .utf8_text(src.as_bytes())
                                .unwrap_or_default()
                                .to_string(),
                        )))
                    }
                    "interpolation" => match child.named_child(0) {
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
        "binary_operator" => {
            let op = node.child_by_field_name("operator").map(|op| text(op, src));
            if op.as_deref() != Some("+") {
                out.push(WrapperUrlPiece::Part(UrlPart::Dynamic));
                return;
            }
            for field in ["left", "right"] {
                match node.child_by_field_name(field) {
                    Some(side) => fold_wrapper_py_url_expr(side, src, param, out),
                    None => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
                }
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => fold_wrapper_py_url_expr(inner, src, param, out),
            None => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
        },
        "identifier" | "attribute" => {
            out.push(WrapperUrlPiece::Part(UrlPart::ConstRef(text(node, src))))
        }
        _ => out.push(WrapperUrlPiece::Part(UrlPart::Dynamic)),
    }
}

/// Does any import in this file bind `obj` as a module? True for an aliased
/// import (`import a.b as obj`), the full dotted receiver (`a.b` written out),
/// or the last segment of a plain dotted import (belt-and-braces — such
/// provisional sites can only drop at resolve, never mis-join).
pub(super) fn py_import_binds_module(imports: &[RawImport], obj_kind: &str, obj: &str) -> bool {
    imports.iter().filter(|imp| !imp.is_static).any(|imp| {
        imp.alias.as_deref() == Some(obj)
            || (obj_kind == "attribute" && imp.alias.is_none() && imp.raw == obj)
            || (obj_kind == "identifier"
                && imp.alias.is_none()
                && imp.raw.rsplit('.').next() == Some(obj))
    })
}

/// URL-ish gate for provisional wrapper calls: the folded first part must be
/// a `Lit` starting with `/`. Keeps `t("common.x")` / `helper(x)` out.
pub(super) fn py_arg_is_url_ish(node: TsNode<'_>, src: &str) -> bool {
    let mut parts = Vec::new();
    fold_py_url_expr(node, src, &mut parts);
    matches!(parts.first(), Some(UrlPart::Lit(lit)) if lit.starts_with('/'))
}




// ── Outbound HTTP contract sites (requests / httpx module-receiver calls) ────
//
// Tight recognizer to avoid false positives: the receiver must be the literal
// module name `requests` or `httpx` — instance clients (`session.get`,
// `client.get(...)`) are out of scope v1. URLs reuse the Phase B parts model:
// f-string interpolations become `Dynamic` parts and fold to `{*}` at resolve.

pub(super) fn python_http_verb(attr: &str) -> Option<&'static str> {
    match attr {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        "head" => Some("HEAD"),
        _ => None,
    }
}

pub(super) fn try_emit_http_contract(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing: Option<(&NodeId, &str)>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    // Bare-identifier callee (`api_get('/x')`) MAY be a same-repo HTTP
    // wrapper. Emit a PROVISIONAL site (placeholder GET — the resolve join
    // overrides with the wrapper's fixed verb); non-matches vanish at resolve.
    if func.kind() == "identifier" {
        let Some(arg0) = positional_argument(node, 0) else {
            return;
        };
        if !py_arg_is_url_ish(arg0, src) {
            return;
        }
        let mut parts = Vec::new();
        fold_py_url_expr(arg0, src, &mut parts);
        if parts.is_empty() {
            return;
        }
        let in_callable = match enclosing {
            Some((id, _)) => id.clone(),
            None => file_id(&builder.rel),
        };
        builder.contract_sites.push(ContractSite {
            kind: ContractKind::HttpCall,
            url_template: None,
            topic: None,
            http_method: Some("GET".into()),
            messaging_framework: None,
            url_parts: Some(parts),
            via_wrapper: Some(text(func, src)),
            in_callable,
            range: range_of(node),
        });
        return;
    }
    if func.kind() != "attribute" {
        return;
    }
    let Some(obj) = func.child_by_field_name("object") else {
        return;
    };
    let obj_text = text(obj, src);
    if obj.kind() != "identifier" || !matches!(obj_text.as_str(), "requests" | "httpx") {
        // Module-attribute wrapper candidate (`api.api_get("/x")` via
        // `import services.api_client as api`, or the full dotted receiver
        // `services.api_client.api_get(...)`). Gated on a known import
        // binding in this file — arbitrary `obj.method(...)` calls never
        // emit. The resolve join pins the module; non-matches vanish.
        if !py_import_binds_module(&builder.imports, obj.kind(), &obj_text) {
            return;
        }
        let attr = func
            .child_by_field_name("attribute")
            .map(|n| text(n, src))
            .unwrap_or_default();
        if attr.is_empty() {
            return;
        }
        let Some(arg0) = positional_argument(node, 0) else {
            return;
        };
        if !py_arg_is_url_ish(arg0, src) {
            return;
        }
        let mut parts = Vec::new();
        fold_py_url_expr(arg0, src, &mut parts);
        if parts.is_empty() {
            return;
        }
        let in_callable = match enclosing {
            Some((id, _)) => id.clone(),
            None => file_id(&builder.rel),
        };
        builder.contract_sites.push(ContractSite {
            kind: ContractKind::HttpCall,
            url_template: None,
            topic: None,
            http_method: Some("GET".into()),
            messaging_framework: None,
            url_parts: Some(parts),
            via_wrapper: Some(format!("{obj_text}.{attr}")),
            in_callable,
            range: range_of(node),
        });
        return;
    }
    let attr = func
        .child_by_field_name("attribute")
        .map(|n| text(n, src))
        .unwrap_or_default();

    let (http_method, url_node) = if let Some(verb) = python_http_verb(&attr) {
        let Some(url) = positional_argument(node, 0) else {
            return;
        };
        (verb.to_string(), url)
    } else if attr == "request" {
        // requests.request("POST", url)
        let Some(method) = positional_argument(node, 0)
            .filter(|arg| arg.kind() == "string")
            .and_then(|arg| literal_py_string(arg, src))
        else {
            return;
        };
        let Some(url) = positional_argument(node, 1) else {
            return;
        };
        (method.to_ascii_uppercase(), url)
    } else {
        return;
    };

    let (in_callable, _) = match enclosing {
        Some((id, fqcn)) => (id.clone(), fqcn.to_string()),
        None => (file_id(&builder.rel), builder.module.clone()),
    };
    let url_template = literal_py_string(url_node, src).map(|url| normalize_external_url(&url));
    let url_parts = if url_template.is_none() {
        py_url_parts(url_node, src)
    } else {
        None
    };
    if url_template.is_none() && url_parts.is_none() {
        return;
    }
    builder.contract_sites.push(ContractSite {
        kind: ContractKind::HttpCall,
        url_template,
        topic: None,
        http_method: Some(http_method),
        messaging_framework: None,
        url_parts,
        via_wrapper: None,
        in_callable,
        range: range_of(node),
    });
}


/// Text of a plain (non-f-string, no-interpolation) string literal.
/// Module-level `X = <init>` → `StringConstant` when the initializer is a
/// plain string literal, or an env-override with a literal default
/// (`x or "/api"`, `os.environ.get("K", "/api")`, `os.getenv("K", "/api")`) —
/// the default becomes the value with `env_default: true`. Anything else
/// emits nothing (references degrade to `{*}`, never a guess).
pub(super) fn collect_module_string_constant(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    if left.kind() != "identifier" {
        return;
    }
    let Some(right) = node.child_by_field_name("right") else {
        return;
    };
    let (value, env_default) = match right.kind() {
        "string" => match literal_py_string(right, src) {
            Some(value) => (value, false),
            None => return,
        },
        "boolean_operator" => {
            let op = right.child_by_field_name("operator").map(|op| text(op, src));
            let default = right
                .child_by_field_name("right")
                .and_then(|r| literal_py_string(r, src));
            match (op.as_deref(), default) {
                (Some("or"), Some(value)) => (value, true),
                _ => return,
            }
        }
        "call" => {
            let callee = right
                .child_by_field_name("function")
                .map(|f| text(f, src))
                .unwrap_or_default();
            if callee != "os.environ.get" && callee != "os.getenv" {
                return;
            }
            let default = right
                .child_by_field_name("arguments")
                .and_then(|args| args.named_child(1))
                .and_then(|arg| literal_py_string(arg, src));
            match default {
                Some(value) => (value, true),
                None => return,
            }
        }
        _ => return,
    };
    builder.string_constants.push(StringConstant {
        const_name: text(left, src),
        owner_fqcn: builder.module.clone(),
        value,
        dynamic: false,
        env_default,
        range: range_of(node),
    });
}


/// Phase B parts for a non-literal URL argument: f-string content → `Lit`,
/// a `{IDENT}` interpolation → `ConstRef` (resolved via module constants and
/// the gated unique-name fallback), other interpolations → `Dynamic`,
/// `+`-concat folds recursively, identifiers → `ConstRef` (unresolved refs
/// degrades them to `{*}` — never a wrong match).
pub(super) fn py_url_parts(node: TsNode<'_>, src: &str) -> Option<Vec<UrlPart>> {
    let mut parts = Vec::new();
    fold_py_url_expr(node, src, &mut parts);
    parts
        .iter()
        .any(|part| !matches!(part, UrlPart::Lit(_)))
        .then_some(parts)
}

pub(super) fn fold_py_url_expr(node: TsNode<'_>, src: &str, out: &mut Vec<UrlPart>) {
    match node.kind() {
        "string" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_content" | "escape_sequence" => out.push(UrlPart::Lit(
                        child.utf8_text(src.as_bytes()).unwrap_or_default().to_string(),
                    )),
                    // `{API_BASE}` → ConstRef; SCREAMING_SNAKE identifiers
                    // only — locals (`{item_id}`) and attribute chains
                    // (`{settings.base}`) stay Dynamic so they can never feed
                    // the cross-file unique-name fallback.
                    "interpolation" => match child.named_child(0) {
                        Some(inner)
                            if inner.kind() == "identifier"
                                && crate::contracts_common::is_screaming_snake(&text(
                                    inner, src,
                                )) =>
                        {
                            out.push(UrlPart::ConstRef(text(inner, src)))
                        }
                        _ => out.push(UrlPart::Dynamic),
                    },
                    // string_start / string_end delimiters
                    _ => {}
                }
            }
        }
        "binary_operator" => {
            let op = node.child_by_field_name("operator").map(|op| text(op, src));
            if op.as_deref() != Some("+") {
                out.push(UrlPart::Dynamic);
                return;
            }
            match node.child_by_field_name("left") {
                Some(left) => fold_py_url_expr(left, src, out),
                None => out.push(UrlPart::Dynamic),
            }
            match node.child_by_field_name("right") {
                Some(right) => fold_py_url_expr(right, src, out),
                None => out.push(UrlPart::Dynamic),
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => fold_py_url_expr(inner, src, out),
            None => out.push(UrlPart::Dynamic),
        },
        "identifier" | "attribute" => out.push(UrlPart::ConstRef(text(node, src))),
        _ => out.push(UrlPart::Dynamic),
    }
}

/// The signature fqcn (`container#name/arity`) for a function/method being emitted — the same
/// string as its node's `qualified_name`, used as `in_fqcn` for calls made inside it.
pub(super) fn callable_fqcn(builder: &Builder, class_fqn: Option<&str>, name: &str, arity: u16) -> String {
    let container = class_fqn.unwrap_or(&builder.module);
    format!("{container}#{name}/{arity}")
}

