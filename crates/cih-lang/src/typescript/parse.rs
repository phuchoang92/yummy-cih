use cih_core::{
    file_id, function_id, type_id, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit,
    Range, RawImport, RefKind, ReferenceSite, RouteSource, SymbolDef,
};
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
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'\'' || first == b'"' || first == b'`') && first == last {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn module_path(rel: &str) -> String {
    let stripped = rel
        .strip_suffix(".tsx")
        .or_else(|| rel.strip_suffix(".ts"))
        .unwrap_or(rel);
    stripped.to_string()
}

fn parameter_count(node: TsNode<'_>) -> u16 {
    let params = node.child_by_field_name("parameters");
    let Some(params) = params else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "required_parameter"
            | "optional_parameter"
            | "rest_pattern"
            | "assignment_pattern" => {
                count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    count
}

// ── decorator helpers ─────────────────────────────────────────────────────────

/// Returns (decorator_name, optional_first_string_arg) for a `decorator` node.
fn decorator_info(node: TsNode<'_>, src: &str) -> Option<(String, Option<String>)> {
    // decorator → `@` + (call_expression | identifier | member_expression)
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "call_expression" => {
                let func = child.child_by_field_name("function")?;
                let name = text(func, src);
                // strip leading `@` from name if present
                let name = name.trim_start_matches('@').to_string();
                let arg = first_string_arg_in_call(child, src);
                return Some((name, arg));
            }
            "identifier" => {
                let name = text(child, src)
                    .trim_start_matches('@')
                    .to_string();
                return Some((name, None));
            }
            _ => {}
        }
    }
    None
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

/// Collect all decorators that appear before a declaration node (siblings).
fn collect_sibling_decorators<'a>(node: TsNode<'a>, src: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let Some(parent) = node.parent() else {
        return out;
    };
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.id() == node.id() {
            break;
        }
        if child.kind() == "decorator" {
            if let Some(info) = decorator_info(child, src) {
                out.push(info);
            }
        }
    }
    out
}

// ── NestJS HTTP verb detection ────────────────────────────────────────────────

fn nestjs_http_method(decorator_name: &str) -> Option<&'static str> {
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

fn express_http_method(method: &str) -> Option<&'static str> {
    match method {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        _ => None,
    }
}

// ── Main builder ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct Builder {
    rel: String,
    module: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    defs: Vec<SymbolDef>,
    imports: Vec<RawImport>,
    reference_sites: Vec<ReferenceSite>,
}

impl Builder {
    fn emit_class(
        &mut self,
        node: TsNode<'_>,
        _src: &str,
        class_name: &str,
        decorators: &[(String, Option<String>)],
    ) -> String {
        let fqn = format!("{}.{}", self.module, class_name);
        let id = type_id(NodeKind::Class, &fqn);
        let range = range_of(node);

        // Check for NestJS stereotype
        let stereotype = if decorators.iter().any(|(n, _)| n == "Controller") {
            Some("nestjs_controller".to_string())
        } else if decorators.iter().any(|(n, _)| n == "Injectable") {
            Some("nestjs_injectable".to_string())
        } else {
            None
        };

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Class,
            name: class_name.to_string(),
            qualified_name: Some(fqn.clone()),
            file: self.rel.clone(),
            range,
            props: stereotype
                .as_deref()
                .map(|s| serde_json::json!({ "stereotype": s })),
        });
        self.edges.push(Edge {
            src: file_id(&self.rel),
            dst: id.clone(),
            kind: EdgeKind::Contains,
            confidence: 1.0,
            reason: "file-type".into(),
        });
        self.defs.push(SymbolDef {
            id,
            kind: NodeKind::Class,
            fqcn: fqn.clone(),
            name: class_name.to_string(),
            owner: None,
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            stereotype: stereotype.map(|s| s.to_string()),
        });
        fqn
    }

    fn emit_interface(&mut self, node: TsNode<'_>, _src: &str, name: &str) {
        let fqn = format!("{}.{}", self.module, name);
        let id = type_id(NodeKind::Interface, &fqn);
        let range = range_of(node);
        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Interface,
            name: name.to_string(),
            qualified_name: Some(fqn.clone()),
            file: self.rel.clone(),
            range,
            props: None,
        });
        self.edges.push(Edge {
            src: file_id(&self.rel),
            dst: id.clone(),
            kind: EdgeKind::Contains,
            confidence: 1.0,
            reason: "file-type".into(),
        });
        self.defs.push(SymbolDef {
            id,
            kind: NodeKind::Interface,
            fqcn: fqn,
            name: name.to_string(),
            owner: None,
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            stereotype: None,
        });
    }

    fn emit_function(
        &mut self,
        node: TsNode<'_>,
        _src: &str,
        name: &str,
        arity: u16,
        owner_fqn: Option<&str>,
    ) -> NodeId {
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

        if let Some(ref owner_id) = owner_id {
            self.edges.push(Edge {
                src: owner_id.clone(),
                dst: id.clone(),
                kind: EdgeKind::HasMethod,
                confidence: 1.0,
                reason: "member".into(),
            });
        } else {
            self.edges.push(Edge {
                src: file_id(&self.rel),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-fn".into(),
            });
        }

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
            stereotype: None,
        });
        id
    }

    fn emit_nestjs_route(
        &mut self,
        fn_node: TsNode<'_>,
        fn_id: &NodeId,
        http_method: &str,
        full_path: &str,
        verb_name: &str,
    ) {
        let route_id = NodeId::new(format!("Route:nestjs:{http_method}:{full_path}"));
        let name = format!("{http_method} {full_path}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(fn_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": full_path,
                "route_annotations": [verb_name],
                "source": RouteSource::NestJs,
                "handler": fn_id.as_str(),
            })),
        });
        self.edges.push(Edge {
            src: fn_id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason: format!("nestjs-{}", http_method.to_ascii_lowercase()),
        });
    }

    fn emit_express_route(
        &mut self,
        call_node: TsNode<'_>,
        http_method: &str,
        path: &str,
    ) {
        let route_id = NodeId::new(format!("Route:express:{http_method}:{path}"));
        let name = format!("{http_method} {path}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(call_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": path,
                "route_annotations": [],
                "source": RouteSource::Express,
            })),
        });
        // No handler edge — we don't resolve the handler function here
        let _ = route_id;
    }

    fn emit_import(&mut self, node: TsNode<'_>, src: &str) {
        // import_statement → `from` "path" + named/namespace/default imports
        // We record the module path as the raw import
        let mut from_path = None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "string" {
                from_path = Some(unquote(&text(child, src)));
            }
        }
        let raw = from_path.unwrap_or_else(|| text(node, src));
        self.imports.push(RawImport {
            raw,
            is_static: false,
            is_wildcard: false,
            range: range_of(node),
        });
    }

    fn emit_call_reference(&mut self, node: TsNode<'_>, src: &str) {
        // call_expression → function: (member_expression | identifier)
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let (name, receiver) = match func.kind() {
            "member_expression" => {
                let obj = func.child_by_field_name("object").map(|n| text(n, src));
                let prop = func
                    .child_by_field_name("property")
                    .map(|n| text(n, src))
                    .unwrap_or_default();
                (prop, obj)
            }
            "identifier" => (text(func, src), None),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        self.reference_sites.push(ReferenceSite {
            name,
            receiver,
            kind: RefKind::Call,
            arity: call_arity(node),
            range: range_of(func),
            in_fqcn: self.module.clone(),
            in_callable: file_id(&self.rel),
        });
    }
}

fn call_arity(node: TsNode<'_>) -> Option<u16> {
    let args = node.child_by_field_name("arguments")?;
    let mut count = 0u16;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        match child.kind() {
            "comment" => {}
            _ => count = count.saturating_add(1),
        }
    }
    Some(count)
}

// ── Recursive AST walker ──────────────────────────────────────────────────────

fn walk(node: TsNode<'_>, src: &str, builder: &mut Builder, class_fqn: Option<&str>, controller_prefix: Option<&str>) {
    match node.kind() {
        "program" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, None, None);
            }
        }
        "export_statement" => {
            // export default class / export function / export const ...
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let class_name = text(name_node, src);
            if class_name.is_empty() {
                return;
            }
            let decorators = collect_sibling_decorators(node, src);
            // Find @Controller prefix if present
            let ctrl_prefix = decorators
                .iter()
                .find(|(n, _)| n == "Controller")
                .and_then(|(_, path)| path.clone())
                .unwrap_or_default();

            let fqn = builder.emit_class(node, src, &class_name, &decorators);

            // Walk body
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, Some(&fqn), Some(&ctrl_prefix));
                }
            }
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, src);
                if !name.is_empty() {
                    builder.emit_interface(node, src, &name);
                }
            }
        }
        "function_declaration" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let name = text(name_node, src);
            if name.is_empty() {
                return;
            }
            let arity = parameter_count(node);
            let decorators = collect_sibling_decorators(node, src);
            let fn_id = builder.emit_function(node, src, &name, arity, class_fqn);

            // Check NestJS decorators
            let ctrl_prefix = controller_prefix.unwrap_or("");
            for (dname, dpath) in &decorators {
                if let Some(http_method) = nestjs_http_method(dname) {
                    let method_path = dpath.as_deref().unwrap_or("");
                    let full_path = join_paths(ctrl_prefix, method_path);
                    builder.emit_nestjs_route(node, &fn_id, http_method, &full_path, dname);
                }
            }

            // Walk body for call references
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, class_fqn, controller_prefix);
                }
            }
        }
        "method_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let name = text(name_node, src);
            if name.is_empty() {
                return;
            }
            let arity = parameter_count(node);
            let decorators = collect_sibling_decorators(node, src);
            let fn_id = builder.emit_function(node, src, &name, arity, class_fqn);

            // Check NestJS method decorators
            let ctrl_prefix = controller_prefix.unwrap_or("");
            for (dname, dpath) in &decorators {
                if let Some(http_method) = nestjs_http_method(dname) {
                    let method_path = dpath.as_deref().unwrap_or("");
                    let full_path = join_paths(ctrl_prefix, method_path);
                    builder.emit_nestjs_route(node, &fn_id, http_method, &full_path, dname);
                }
            }

            // Walk body
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, class_fqn, controller_prefix);
                }
            }
        }
        "import_statement" => {
            builder.emit_import(node, src);
        }
        "call_expression" => {
            // Check for Express-style routes: app.get('/path', ...) / router.post(...)
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "member_expression" {
                    if let Some(obj) = func.child_by_field_name("object") {
                        let obj_name = text(obj, src);
                        if matches!(obj_name.as_str(), "app" | "router" | "express") {
                            if let Some(prop) = func.child_by_field_name("property") {
                                let method = text(prop, src);
                                if let Some(http_method) = express_http_method(&method) {
                                    // first argument is the path
                                    if let Some(args) = node.child_by_field_name("arguments") {
                                        let mut cursor = args.walk();
                                        for arg in args.named_children(&mut cursor) {
                                            if arg.kind() == "string" {
                                                let path = unquote(&text(arg, src));
                                                builder.emit_express_route(node, http_method, &path);
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            builder.emit_call_reference(node, src);
            // recurse into arguments
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix);
            }
        }
    }
}

fn join_paths(prefix: &str, suffix: &str) -> String {
    let p = prefix.trim_matches('/');
    let s = suffix.trim_matches('/');
    if p.is_empty() {
        format!("/{s}")
    } else if s.is_empty() {
        format!("/{p}")
    } else {
        format!("/{p}/{s}")
    }
}

pub fn parse_typescript_file(rel: &str, src: &str) -> anyhow::Result<ParsedUnit> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .expect("TypeScript language must load");

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
                },
            });
        }
    };

    let module = module_path(rel);
    let mut builder = Builder {
        rel: rel.to_string(),
        module,
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
            contract_sites: Vec::new(),
            sql_constants: Vec::new(),
            sql_execution_sites: Vec::new(),
        },
    })
}
