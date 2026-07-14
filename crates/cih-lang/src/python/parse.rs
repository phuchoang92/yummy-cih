use cih_core::{
    file_id, function_id, type_id, ContractKind, ContractSite, Edge, EdgeKind, Node, NodeId,
    NodeKind, ParsedFile, ParsedUnit, Range, RawImport, RefKind, ReferenceSite, RouteSource,
    HttpWrapperDef, StringConstant, SymbolDef, UrlPart,
};
use crate::contracts_common::normalize_external_url;
use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_python};
use std::collections::HashMap;
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

fn unquote(raw: &str) -> String {
    let s = raw.trim();
    // Handle triple-quoted strings
    for delim in &["\"\"\"", "'''"] {
        if s.starts_with(delim) && s.ends_with(delim) && s.len() >= 6 {
            return s[3..s.len() - 3].to_string();
        }
    }
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'\'' || first == b'"') && first == last {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn module_path(rel: &str) -> String {
    let stripped = rel.strip_suffix(".py").unwrap_or(rel);
    stripped.replace(['/', '\\'], ".")
}

// ── HTTP wrapper detection (python analog of the TS apiFetch pattern) ────────

/// One piece of a candidate wrapper's URL expression: a regular part, or the
/// pass-through parameter slot.
enum WrapperUrlPiece {
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
fn try_collect_py_http_wrapper(name: &str, fn_node: TsNode<'_>, src: &str, builder: &mut Builder) {
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
fn first_py_param_identifier(fn_node: TsNode<'_>, src: &str) -> Option<String> {
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
fn find_inner_py_http_call<'a>(body: TsNode<'a>, src: &str) -> Option<TsNode<'a>> {
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
fn find_unique_py_assignment<'a>(body: TsNode<'a>, local: &str, src: &str) -> Option<TsNode<'a>> {
    let mut found: Option<TsNode<'a>> = None;
    let mut count = 0u32;
    collect_py_assignments(body, local, src, &mut found, &mut count);
    (count == 1).then_some(found).flatten()
}

fn collect_py_assignments<'a>(
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
fn fold_wrapper_py_url_expr(
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
fn py_import_binds_module(imports: &[RawImport], obj_kind: &str, obj: &str) -> bool {
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
fn py_arg_is_url_ish(node: TsNode<'_>, src: &str) -> bool {
    let mut parts = Vec::new();
    fold_py_url_expr(node, src, &mut parts);
    matches!(parts.first(), Some(UrlPart::Lit(lit)) if lit.starts_with('/'))
}

/// Normalize a relative import (`.api_client`, `..pkg.mod`, `.`) against the
/// importing file's repo-relative path into a dotted absolute module. One
/// package level is stripped per leading dot beyond the first; walking above
/// the repo root returns `None`.
fn normalize_relative_import(spec: &str, rel: &str) -> Option<String> {
    let dots = spec.chars().take_while(|c| *c == '.').count();
    if dots == 0 {
        return None;
    }
    let remainder = &spec[dots..];
    let mut package: Vec<&str> = rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("").split('/').filter(|part| !part.is_empty()).collect();
    for _ in 1..dots {
        package.pop()?;
    }
    for segment in remainder.split('.').filter(|segment| !segment.is_empty()) {
        package.push(segment);
    }
    if package.is_empty() {
        return None;
    }
    Some(package.join("."))
}

fn parameter_count(node: TsNode<'_>) -> u16 {
    let Some(params) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "identifier"
            | "typed_parameter"
            | "typed_default_parameter"
            | "default_parameter"
            | "list_splat_pattern"
            | "dictionary_splat_pattern"
            | "keyword_separator"
            | "positional_separator" => {
                count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    // Subtract self/cls if present
    count.saturating_sub(1)
}

// ── Decorator helpers ─────────────────────────────────────────────────────────

/// Returns (decorator_text, object_part, attr_part, first_string_arg)
/// e.g. `@app.route('/path')` → ("app.route", Some("app"), Some("route"), Some("/path"))
/// e.g. `@router.get('/path')` → ("router.get", Some("router"), Some("get"), Some("/path"))
/// e.g. `@app.get` → ("app.get", Some("app"), Some("get"), None)
fn parse_decorator(node: TsNode<'_>, src: &str) -> Option<DecoratorInfo> {
    // decorator → `@` + expression (identifier | attribute | call)
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        return Some(match child.kind() {
            "call" => {
                let func = child.child_by_field_name("function")?;
                let (obj, attr) = parse_attribute_or_identifier(func, src);
                let first_arg = first_string_arg_in_call(child, src);
                let methods_arg = methods_kwarg_in_call(child, src);
                DecoratorInfo {
                    full: text(func, src),
                    obj,
                    attr,
                    path_arg: first_arg,
                    methods: methods_arg,
                }
            }
            "attribute" => {
                let (obj, attr) = parse_attribute_or_identifier(child, src);
                DecoratorInfo {
                    full: text(child, src),
                    obj,
                    attr,
                    path_arg: None,
                    methods: Vec::new(),
                }
            }
            "identifier" => DecoratorInfo {
                full: text(child, src),
                obj: None,
                attr: Some(text(child, src)),
                path_arg: None,
                methods: Vec::new(),
            },
            _ => continue,
        });
    }
    None
}

struct DecoratorInfo {
    #[allow(dead_code)]
    full: String,
    obj: Option<String>,
    attr: Option<String>,
    path_arg: Option<String>,
    methods: Vec<String>,
}

fn parse_attribute_or_identifier(node: TsNode<'_>, src: &str) -> (Option<String>, Option<String>) {
    if node.kind() == "attribute" {
        let obj = node.child_by_field_name("object").map(|n| text(n, src));
        let attr = node.child_by_field_name("attribute").map(|n| text(n, src));
        (obj, attr)
    } else {
        (None, Some(text(node, src)))
    }
}

fn first_string_arg_in_call(call_node: TsNode<'_>, src: &str) -> Option<String> {
    let args = call_node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() == "string" {
            return Some(unquote(&text(child, src)));
        }
    }
    None
}

fn methods_kwarg_in_call(call_node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(args) = call_node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() != "keyword_argument" {
            continue;
        }
        let key = child.child_by_field_name("name").map(|n| text(n, src));
        if key.as_deref() != Some("methods") {
            continue;
        }
        // value should be a list
        if let Some(value) = child.child_by_field_name("value") {
            let mut vcursor = value.walk();
            for item in value.named_children(&mut vcursor) {
                if item.kind() == "string" {
                    out.push(unquote(&text(item, src)).to_ascii_uppercase());
                }
            }
        }
    }
    out
}

fn kwarg_string_value(call_node: TsNode<'_>, key: &str, src: &str) -> Option<String> {
    let args = call_node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() != "keyword_argument" {
            continue;
        }
        let k = child.child_by_field_name("name").map(|n| text(n, src));
        if k.as_deref() != Some(key) {
            continue;
        }
        if let Some(v) = child.child_by_field_name("value") {
            if v.kind() == "string" {
                return Some(unquote(&text(v, src)));
            }
        }
    }
    None
}

fn normalize_route_path(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let path = path.trim_start_matches('/');
    if prefix.is_empty() {
        format!("/{path}")
    } else if path.is_empty() {
        format!("/{prefix}")
    } else {
        format!("/{prefix}/{path}")
    }
}

fn flask_http_method(attr: &str) -> Option<&'static str> {
    match attr {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        "route" => None, // handled separately via methods kwarg
        _ => None,
    }
}

fn fastapi_http_method(attr: &str) -> Option<&'static str> {
    match attr {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        _ => None,
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Builder {
    rel: String,
    module: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    defs: Vec<SymbolDef>,
    imports: Vec<RawImport>,
    reference_sites: Vec<ReferenceSite>,
    contract_sites: Vec<ContractSite>,
    /// variable_name → url_prefix for FastAPI APIRouter instances
    fastapi_prefixes: HashMap<String, String>,
    /// variable_name → url_prefix for Flask Blueprint instances
    flask_prefixes: HashMap<String, String>,
    /// Whether the file imports FastAPI / Flask — disambiguates `@app.get`-style decorators, which
    /// are valid in both frameworks (mirrors `python/mod.rs` `detect_frameworks`).
    has_fastapi: bool,
    has_flask: bool,
    string_constants: Vec<StringConstant>,
    http_wrappers: Vec<HttpWrapperDef>,
}

impl Builder {
    fn emit_class(&mut self, node: TsNode<'_>, _src: &str, name: &str, owner_fqn: Option<&str>) -> String {
        let fqn = if let Some(owner) = owner_fqn {
            format!("{owner}.{name}")
        } else {
            format!("{}.{}", self.module, name)
        };
        let id = type_id(NodeKind::Class, &fqn);
        let range = range_of(node);

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Class,
            name: name.to_string(),
            qualified_name: Some(fqn.clone()),
            file: self.rel.clone(),
            range,
            props: None,
        });
        let owner_id = owner_fqn.map(|f| type_id(NodeKind::Class, f));
        if let Some(ref oid) = owner_id {
            self.edges.push(Edge {
                src: oid.clone(),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "nested-class".into(),
            props: None,
            });
        } else {
            self.edges.push(Edge {
                src: file_id(&self.rel),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-type".into(),
            props: None,
            });
        }
        self.defs.push(SymbolDef {
            id,
            kind: NodeKind::Class,
            fqcn: fqn.clone(),
            name: name.to_string(),
            owner: owner_id,
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            framework_role: None,
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
        });
        fqn
    }

    fn emit_function(
        &mut self,
        node: TsNode<'_>,
        src: &str,
        name: &str,
        arity: u16,
        owner_fqn: Option<&str>,
    ) -> NodeId {
        let _ = src; // retained for API consistency
        let container_fqn = owner_fqn.unwrap_or(&self.module);
        let id = function_id(container_fqn, name, arity);
        let range = range_of(node);
        let owner_id = owner_fqn.map(|f| type_id(NodeKind::Class, f));

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Function,
            name: name.to_string(),
            qualified_name: Some(format!("{container_fqn}#{name}/{arity}")),
            file: self.rel.clone(),
            range,
            props: None,
        });

        if let Some(ref oid) = owner_id {
            self.edges.push(Edge {
                src: oid.clone(),
                dst: id.clone(),
                kind: EdgeKind::HasMethod,
                confidence: 1.0,
                reason: "member".into(),
            props: None,
            });
        } else {
            self.edges.push(Edge {
                src: file_id(&self.rel),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-fn".into(),
            props: None,
            });
        }

        let body_fingerprint = node
            .child_by_field_name("body")
            .and_then(|b| compute_body_fingerprint(b, "python", normalize_leaf_token_python));
        self.defs.push(SymbolDef {
            id: id.clone(),
            kind: NodeKind::Function,
            fqcn: container_fqn.to_string(),
            name: name.to_string(),
            owner: owner_id,
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            framework_role: None,
            complexity: None,
            body_fingerprint,
            lang_meta: None,
        });
        id
    }

    fn emit_flask_route(
        &mut self,
        fn_node: TsNode<'_>,
        fn_id: &NodeId,
        http_method: &str,
        path: &str,
    ) {
        let route_id = NodeId::new(format!("Route:flask:{}:{}", http_method, path));
        let name = format!("{http_method} {path}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(fn_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": path,
                "route_annotations": [],
                "source": RouteSource::Flask,
                "handler": fn_id.as_str(),
            })),
        });
        self.edges.push(Edge {
            src: fn_id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("flask-{}", http_method.to_ascii_lowercase()),
            props: None,
        });
    }

    fn emit_fastapi_route(
        &mut self,
        fn_node: TsNode<'_>,
        fn_id: &NodeId,
        http_method: &str,
        path: &str,
    ) {
        let route_id = NodeId::new(format!("Route:fastapi:{}:{}", http_method, path));
        let name = format!("{http_method} {path}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(fn_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": path,
                "route_annotations": [],
                "source": RouteSource::FastApi,
                "handler": fn_id.as_str(),
            })),
        });
        self.edges.push(Edge {
            src: fn_id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("fastapi-{}", http_method.to_ascii_lowercase()),
            props: None,
        });
    }

    fn emit_import(&mut self, node: TsNode<'_>, src: &str) {
        // Record DOTTED MODULE paths (`services.api_client`), not statement
        // text — the module string is the cross-file owner key the constant
        // resolver and wrapper index look up. Relative imports normalize
        // against this file's directory; un-normalizable forms record the
        // node text as-is (lookups miss — degrade, never guess).
        let range = range_of(node);
        let mut raws: Vec<(String, bool, Option<String>)> = Vec::new();
        match node.kind() {
            // `import a.b`, `import a.b as c`, `import os, sys` — one entry
            // per imported module.
            "import_statement" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "dotted_name" => raws.push((text(child, src), false, None)),
                        "aliased_import" => {
                            if let Some(name) = child.child_by_field_name("name") {
                                let alias = child
                                    .child_by_field_name("alias")
                                    .map(|alias| text(alias, src));
                                raws.push((text(name, src), false, alias));
                            }
                        }
                        _ => {}
                    }
                }
            }
            // `from a.b import x, y` / `from a.b import *` / `from .x import y`
            // — ONE entry: the source module.
            "import_from_statement" => {
                let mut cursor = node.walk();
                let is_wildcard = node
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "wildcard_import");
                drop(cursor);
                let raw = match node.child_by_field_name("module_name") {
                    Some(module) if module.kind() == "dotted_name" => text(module, src),
                    Some(module) if module.kind() == "relative_import" => {
                        normalize_relative_import(&text(module, src), &self.rel)
                            .unwrap_or_else(|| text(node, src))
                    }
                    _ => text(node, src),
                };
                raws.push((raw, is_wildcard, None));
            }
            _ => raws.push((text(node, src), false, None)),
        }
        if raws.is_empty() {
            raws.push((text(node, src), false, None));
        }
        for (raw, is_wildcard, alias) in raws {
            self.imports.push(RawImport {
                raw,
                is_static: false,
                is_wildcard,
                alias,
                range,
            });
        }
    }

    /// `enclosing` is the function that lexically contains this call — its node id and its
    /// signature fqcn (`container#name/arity`). When present, the call is attributed to that
    /// function so a `Calls` edge originates from the caller; module-level calls (no enclosing
    /// function) fall back to the file / module, as before.
    fn emit_call_reference(
        &mut self,
        node: TsNode<'_>,
        src: &str,
        enclosing: Option<(&NodeId, &str)>,
    ) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let (name, receiver) = match func.kind() {
            "attribute" => {
                let obj = func.child_by_field_name("object").map(|n| text(n, src));
                let attr = func
                    .child_by_field_name("attribute")
                    .map(|n| text(n, src))
                    .unwrap_or_default();
                (attr, obj)
            }
            "identifier" => (text(func, src), None),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        let (in_fqcn, in_callable) = match enclosing {
            Some((id, fqcn)) => (fqcn.to_string(), id.clone()),
            None => (self.module.clone(), file_id(&self.rel)),
        };
        self.reference_sites.push(ReferenceSite {
            name,
            receiver,
            kind: RefKind::Call,
            arity: call_arity(node),
            range: range_of(func),
            in_fqcn,
            in_callable,
            arg_texts: Vec::new(),
        });
    }
}

fn call_arity(node: TsNode<'_>) -> Option<u16> {
    let args = node.child_by_field_name("arguments")?;
    let mut count = 0u16;
    let mut cursor = args.walk();
    for _child in args.named_children(&mut cursor) {
        count = count.saturating_add(1);
    }
    Some(count)
}

// ── Decorator parsing for a function/class ────────────────────────────────────

/// In tree-sitter-python, `decorated_definition` contains decorator children
/// followed by a `definition` child (the actual function/class).
fn collect_decorators_from_decorated(node: TsNode<'_>, src: &str) -> Vec<DecoratorInfo> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "decorator" {
            if let Some(info) = parse_decorator(child, src) {
                out.push(info);
            }
        }
    }
    out
}

fn process_function_decorators(
    fn_node: TsNode<'_>,
    fn_id: &NodeId,
    decorators: &[DecoratorInfo],
    builder: &mut Builder,
) {
    for dec in decorators {
        let obj = dec.obj.as_deref().unwrap_or("");
        let attr = dec.attr.as_deref().unwrap_or("");

        // Clone prefix strings to avoid borrow conflict with mutable emit calls below.
        let fastapi_prefix = builder.fastapi_prefixes.get(obj).cloned();
        let flask_prefix = builder.flask_prefixes.get(obj).cloned();

        // Tracked FastAPI APIRouter with prefix
        if let Some(prefix) = fastapi_prefix {
            if let Some(http_method) = fastapi_http_method(attr) {
                if let Some(ref path) = dec.path_arg {
                    let full_path = normalize_route_path(&prefix, path);
                    builder.emit_fastapi_route(fn_node, fn_id, http_method, &full_path);
                }
                continue;
            }
        }

        // Tracked Flask Blueprint with prefix
        if let Some(prefix) = flask_prefix {
            if attr == "route" {
                if let Some(ref path) = dec.path_arg {
                    let methods = if dec.methods.is_empty() {
                        vec!["GET".to_string()]
                    } else {
                        dec.methods.clone()
                    };
                    let full_path = normalize_route_path(&prefix, path);
                    for method in methods {
                        builder.emit_flask_route(fn_node, fn_id, &method, &full_path);
                    }
                }
                continue;
            }
            if let Some(http_method) = flask_http_method(attr) {
                if let Some(ref path) = dec.path_arg {
                    let full_path = normalize_route_path(&prefix, path);
                    builder.emit_flask_route(fn_node, fn_id, http_method, &full_path);
                }
                continue;
            }
        }

        // Flask: @app.route('/path', methods=['GET', 'POST'])
        if attr == "route" && (obj == "app" || obj == "blueprint") {
            if let Some(ref path) = dec.path_arg {
                let methods = if dec.methods.is_empty() {
                    vec!["GET".to_string()]
                } else {
                    dec.methods.clone()
                };
                for method in methods {
                    builder.emit_flask_route(fn_node, fn_id, &method, path);
                }
            }
            continue;
        }

        // HTTP-method shorthand: @app.get / @app.post / @router.get, etc. This syntax is valid in
        // both FastAPI and Flask 2.0+, so disambiguate by the file's imported framework: `router`
        // is FastAPI-only (APIRouter); for `app`/`blueprint`, prefer FastAPI only when the file
        // imports it and not Flask, else Flask.
        if let Some(http_method) = fastapi_http_method(attr) {
            if obj == "router" || obj == "app" || obj == "blueprint" {
                if let Some(ref path) = dec.path_arg {
                    let is_fastapi =
                        obj == "router" || (builder.has_fastapi && !builder.has_flask);
                    if is_fastapi {
                        builder.emit_fastapi_route(fn_node, fn_id, http_method, path);
                    } else {
                        builder.emit_flask_route(fn_node, fn_id, http_method, path);
                    }
                }
                continue;
            }
        }
    }
}

fn detect_and_store_router_prefix(node: TsNode<'_>, src: &str, builder: &mut Builder) {
    let Some(left) = node.child_by_field_name("left") else { return };
    if left.kind() != "identifier" {
        return;
    }
    let var_name = text(left, src);

    let Some(right) = node.child_by_field_name("right") else { return };
    if right.kind() != "call" {
        return;
    }

    let Some(func) = right.child_by_field_name("function") else { return };
    let func_name = text(func, src);

    match func_name.as_str() {
        "APIRouter" => {
            if let Some(prefix) = kwarg_string_value(right, "prefix", src) {
                builder.fastapi_prefixes.insert(var_name, prefix);
            }
        }
        "Blueprint" => {
            if let Some(prefix) = kwarg_string_value(right, "url_prefix", src) {
                builder.flask_prefixes.insert(var_name, prefix);
            }
        }
        _ => {}
    }
}

// ── AST walker ────────────────────────────────────────────────────────────────

/// `enclosing` is the function that lexically contains `node` (its node id + signature fqcn), or
/// `None` at module / class-body scope. It is threaded so call sites can be attributed to their
/// caller (see [`Builder::emit_call_reference`]).
fn walk(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    class_fqn: Option<&str>,
    enclosing: Option<(&NodeId, &str)>,
) {
    match node.kind() {
        "module" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, None, None);
            }
        }
        "decorated_definition" => {
            let decorators = collect_decorators_from_decorated(node, src);
            // The definition is the last child that is a function or class
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "function_definition" {
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let name = text(name_node, src);
                    if name.is_empty() {
                        continue;
                    }
                    let arity = parameter_count(child);
                    let fn_fqcn = callable_fqcn(builder, class_fqn, &name, arity);
                    let fn_id = builder.emit_function(child, src, &name, arity, class_fqn);
                    if class_fqn.is_none() && enclosing.is_none() {
                        try_collect_py_http_wrapper(&name, child, src, builder);
                    }
                    process_function_decorators(child, &fn_id, &decorators, builder);
                    // Walk body — calls inside attribute to this function.
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut bcursor = body.walk();
                        for bchild in body.named_children(&mut bcursor) {
                            walk(bchild, src, builder, class_fqn, Some((&fn_id, &fn_fqcn)));
                        }
                    }
                } else if child.kind() == "class_definition" {
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let name = text(name_node, src);
                    if name.is_empty() {
                        continue;
                    }
                    let fqn = builder.emit_class(child, src, &name, class_fqn);
                    // Walk body — class scope resets the enclosing function.
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut bcursor = body.walk();
                        for bchild in body.named_children(&mut bcursor) {
                            walk(bchild, src, builder, Some(&fqn), None);
                        }
                    }
                }
            }
        }
        "class_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let name = text(name_node, src);
            if name.is_empty() {
                return;
            }
            let fqn = builder.emit_class(node, src, &name, class_fqn);
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, Some(&fqn), None);
                }
            }
        }
        "function_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let name = text(name_node, src);
            if name.is_empty() {
                return;
            }
            let arity = parameter_count(node);
            let fn_fqcn = callable_fqcn(builder, class_fqn, &name, arity);
            let fn_id = builder.emit_function(node, src, &name, arity, class_fqn);
            if class_fqn.is_none() && enclosing.is_none() {
                try_collect_py_http_wrapper(&name, node, src, builder);
            }
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, class_fqn, Some((&fn_id, &fn_fqcn)));
                }
            }
        }
        "assignment" => {
            detect_and_store_router_prefix(node, src, builder);
            // Module-level `X = "…"` (incl. env-default forms) becomes a
            // StringConstant for cross-file URL folding.
            if class_fqn.is_none() && enclosing.is_none() {
                collect_module_string_constant(node, src, builder);
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, enclosing);
            }
        }
        "import_statement" | "import_from_statement" => {
            builder.emit_import(node, src);
        }
        "call" => {
            try_emit_http_contract(node, src, builder, enclosing);
            builder.emit_call_reference(node, src, enclosing);
            // recurse into arguments — still within the same enclosing function.
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, enclosing);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, enclosing);
            }
        }
    }
}

// ── Outbound HTTP contract sites (requests / httpx module-receiver calls) ────
//
// Tight recognizer to avoid false positives: the receiver must be the literal
// module name `requests` or `httpx` — instance clients (`session.get`,
// `client.get(...)`) are out of scope v1. URLs reuse the Phase B parts model:
// f-string interpolations become `Dynamic` parts and fold to `{*}` at resolve.

fn python_http_verb(attr: &str) -> Option<&'static str> {
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

fn try_emit_http_contract(
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

/// Nth positional (non-keyword) argument of a call.
fn positional_argument(call: TsNode<'_>, n: usize) -> Option<TsNode<'_>> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut index = 0;
    for child in args.named_children(&mut cursor) {
        if matches!(child.kind(), "keyword_argument" | "comment") {
            continue;
        }
        if index == n {
            return Some(child);
        }
        index += 1;
    }
    None
}

/// Text of a plain (non-f-string, no-interpolation) string literal.
/// Module-level `X = <init>` → `StringConstant` when the initializer is a
/// plain string literal, or an env-override with a literal default
/// (`x or "/api"`, `os.environ.get("K", "/api")`, `os.getenv("K", "/api")`) —
/// the default becomes the value with `env_default: true`. Anything else
/// emits nothing (references degrade to `{*}`, never a guess).
fn collect_module_string_constant(node: TsNode<'_>, src: &str, builder: &mut Builder) {
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

fn literal_py_string(node: TsNode<'_>, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "interpolation")
    {
        return None;
    }
    let raw = text(node, src);
    let stripped = raw
        .strip_prefix(|c: char| matches!(c, 'f' | 'F' | 'r' | 'R' | 'b' | 'B' | 'u' | 'U'))
        .unwrap_or(&raw);
    Some(unquote(stripped))
}

/// Phase B parts for a non-literal URL argument: f-string content → `Lit`,
/// a `{IDENT}` interpolation → `ConstRef` (resolved via module constants and
/// the gated unique-name fallback), other interpolations → `Dynamic`,
/// `+`-concat folds recursively, identifiers → `ConstRef` (unresolved refs
/// degrades them to `{*}` — never a wrong match).
fn py_url_parts(node: TsNode<'_>, src: &str) -> Option<Vec<UrlPart>> {
    let mut parts = Vec::new();
    fold_py_url_expr(node, src, &mut parts);
    parts
        .iter()
        .any(|part| !matches!(part, UrlPart::Lit(_)))
        .then_some(parts)
}

fn fold_py_url_expr(node: TsNode<'_>, src: &str, out: &mut Vec<UrlPart>) {
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
fn callable_fqcn(builder: &Builder, class_fqn: Option<&str>, name: &str, arity: u16) -> String {
    let container = class_fqn.unwrap_or(&builder.module);
    format!("{container}#{name}/{arity}")
}

pub fn parse_python_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("Python language must load");

    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => {
            return Ok(ParsedUnit {
                rel: rel.to_string(),
                nodes: Vec::new(),
                edges: Vec::new(),
                parsed_file: ParsedFile {
                    file: rel.to_string(),
                    language: String::new(),
                    package: None,
                    defs: Vec::new(),
                    imports: Vec::new(),
                    reference_sites: Vec::new(),
                    type_bindings: Vec::new(),
                    contract_sites: Vec::new(),
                    sql_constants: Vec::new(),
                    sql_execution_sites: Vec::new(),
                    string_constants: Vec::new(),
                http_wrappers: Vec::new(),
            },
            });
        }
    };

    let module = module_path(rel);
    let mut builder = Builder {
        rel: rel.to_string(),
        module,
        has_fastapi: src.contains("from fastapi") || src.contains("import fastapi"),
        has_flask: src.contains("from flask") || src.contains("import flask"),
        ..Builder::default()
    };

    walk(tree.root_node(), src, &mut builder, None, None);

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        parsed_file: ParsedFile {
            file: rel.to_string(),
            language: String::new(),
            package: None,
            defs: builder.defs,
            imports: builder.imports,
            reference_sites: builder.reference_sites,
            type_bindings: Vec::new(),
            contract_sites: builder.contract_sites,
            sql_constants: Vec::new(),
            sql_execution_sites: Vec::new(),
            string_constants: builder.string_constants,
            http_wrappers: builder.http_wrappers,
    },
    })
}

