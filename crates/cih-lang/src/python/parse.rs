use cih_core::{
    NodeId, ParsedFile, ParsedUnit,
};
use tree_sitter::Node as TsNode;

use super::builder::Builder;
use super::helpers::*;
use super::http_clients::*;





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

