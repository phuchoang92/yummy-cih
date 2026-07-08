use cih_core::{
    file_id, function_id, type_id, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit,
    Range, RawImport, RefKind, ReferenceSite, RouteSource, SymbolDef,
};
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
    /// variable_name → url_prefix for FastAPI APIRouter instances
    fastapi_prefixes: HashMap<String, String>,
    /// variable_name → url_prefix for Flask Blueprint instances
    flask_prefixes: HashMap<String, String>,
    /// Whether the file imports FastAPI / Flask — disambiguates `@app.get`-style decorators, which
    /// are valid in both frameworks (mirrors `python/mod.rs` `detect_frameworks`).
    has_fastapi: bool,
    has_flask: bool,
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
        // import_statement: `import X` or `from X import Y`
        // import_from_statement: `from module import names`
        let raw = text(node, src);
        self.imports.push(RawImport {
            raw,
            is_static: false,
            is_wildcard: false,
            range: range_of(node),
        });
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
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, class_fqn, Some((&fn_id, &fn_fqcn)));
                }
            }
        }
        "assignment" => {
            detect_and_store_router_prefix(node, src, builder);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, enclosing);
            }
        }
        "import_statement" | "import_from_statement" => {
            builder.emit_import(node, src);
        }
        "call" => {
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
                import_bindings: Vec::new(),
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

    // Convert RawImports to ImportBindings (best-effort for Python)
    let import_bindings = builder.imports.iter().map(|imp| {
        use cih_core::{ImportBinding, ImportBindingKind};
        ImportBinding {
            module: imp.raw.clone(),
            imported: None,
            local: None,
            kind: if imp.is_wildcard { ImportBindingKind::Wildcard } else { ImportBindingKind::Named },
            range: imp.range,
        }
    }).collect::<Vec<_>>();

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        import_bindings,
        parsed_file: ParsedFile {
            file: rel.to_string(),
            language: String::new(),
            package: None,
            defs: builder.defs,
            imports: builder.imports,
            reference_sites: builder.reference_sites,
            type_bindings: Vec::new(),
            contract_sites: Vec::new(),
            sql_constants: Vec::new(),
            sql_execution_sites: Vec::new(),
            string_constants: Vec::new(),
        },
    })
}

