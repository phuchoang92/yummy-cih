use cih_core::{
    file_id, function_id, type_id, ContractKind, ContractSite, Edge, EdgeKind, Node, NodeId,
    NodeKind, ParsedFile, ParsedUnit, Range, RawImport, RefKind, ReferenceSite, RouteSource,
    HttpWrapperDef, StringConstant, SymbolDef, UrlPart,
};
use crate::contracts_common::normalize_external_url;
use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_typescript};
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
    // TypeScript + JavaScript extensions (longest/most-specific first).
    for ext in [".tsx", ".jsx", ".mjs", ".cjs", ".ts", ".js"] {
        if let Some(stripped) = rel.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    rel.to_string()
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

/// Collect the decorators that decorate `node` — the contiguous run of
/// `decorator` siblings immediately preceding it. Resetting on any other sibling
/// (e.g. a previous method) is essential: without it, later members in a class
/// body inherit earlier members' decorators (duplicate routes / contracts).
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
        match child.kind() {
            "decorator" => {
                if let Some(info) = decorator_info(child, src) {
                    out.push(info);
                }
            }
            "comment" => {} // decorators may be interleaved with comments
            _ => out.clear(), // a non-decorator sibling ends the run
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

// ── Additional backend route frameworks (Fastify / Koa / Hapi) ─────────────────

/// Lower-case HTTP verb → canonical method, covering the broader set Fastify/Koa
/// expose (`head`/`options`/`all`) on top of Express's five.
fn any_http_verb(method: &str) -> Option<&'static str> {
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
fn route_source_label(source: RouteSource) -> &'static str {
    match source {
        RouteSource::Express => "express",
        RouteSource::NestJs => "nestjs",
        RouteSource::Fastify => "fastify",
        RouteSource::Koa => "koa",
        RouteSource::Hapi => "hapi",
        RouteSource::NextJs => "nextjs",
        RouteSource::Remix => "remix",
        _ => "route",
    }
}

/// GraphQL resolver operation from a `@Query`/`@Mutation`/`@Subscription`
/// decorator (type-graphql / NestJS).
fn graphql_operation(dname: &str) -> Option<&'static str> {
    match dname {
        "Query" => Some("QUERY"),
        "Mutation" => Some("MUTATION"),
        "Subscription" => Some("SUBSCRIPTION"),
        _ => None,
    }
}

/// Value node of `{ key: value }` pair `key_name` in an `object` literal.
fn object_pair_value<'a>(obj: TsNode<'a>, key_name: &str, src: &str) -> Option<TsNode<'a>> {
    let mut cursor = obj.walk();
    for entry in obj.named_children(&mut cursor) {
        if entry.kind() != "pair" {
            continue;
        }
        let key = entry.child_by_field_name("key").map(|n| unquote(&text(n, src)));
        if key.as_deref() == Some(key_name) {
            return entry.child_by_field_name("value");
        }
    }
    None
}

/// HTTP method(s) from a config object's `method` value — a string (`'GET'`) or
/// an array (`['GET','POST']`). Upper-cased.
fn config_route_methods(obj: TsNode<'_>, src: &str) -> Vec<String> {
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
fn emit_config_routes(
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
fn detect_call_route(node: TsNode<'_>, src: &str, builder: &mut Builder) {
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

// ── File-based routes (Next.js / Remix) ───────────────────────────────────────

/// Top-level exported names (functions + `export const`), used to detect
/// App-Router verb handlers (`export function GET`) and Remix `loader`/`action`.
fn exported_top_level_names(root: TsNode<'_>, src: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "export_statement" {
            continue;
        }
        let mut c2 = child.walk();
        for inner in child.named_children(&mut c2) {
            match inner.kind() {
                "function_declaration" | "generator_function_declaration" => {
                    if let Some(n) = inner.child_by_field_name("name") {
                        out.insert(text(n, src));
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    let mut c3 = inner.walk();
                    for d in inner.named_children(&mut c3) {
                        if d.kind() == "variable_declarator" {
                            if let Some(n) = d.child_by_field_name("name") {
                                out.insert(text(n, src));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `[id]` → `:id`, `[...slug]`/`[[...slug]]` → `:slug` (Next.js dynamic segments).
fn next_dynamic_segment(seg: &str) -> String {
    let inner = seg.trim_start_matches('[').trim_end_matches(']');
    if inner.len() != seg.len() {
        format!(":{}", inner.trim_start_matches("..."))
    } else {
        seg.to_string()
    }
}

/// Substring after a `pages/api/` path boundary, if `norm` is a Next.js pages API file.
fn pages_api_subpath(norm: &str) -> Option<&str> {
    let idx = norm.find("pages/api/")?;
    if idx != 0 && norm.as_bytes()[idx - 1] != b'/' {
        return None;
    }
    Some(&norm[idx + "pages/api/".len()..])
}

/// Next.js pages API file path → HTTP path (e.g. `users/[id].ts` → `/api/users/:id`).
fn next_pages_api_path(rest: &str) -> String {
    let stem = module_path(rest);
    let stem = stem.strip_suffix("/index").unwrap_or(&stem);
    let stem = if stem == "index" { "" } else { stem };
    let mut p = String::from("/api");
    for seg in stem.split('/').filter(|s| !s.is_empty()) {
        p.push('/');
        p.push_str(&next_dynamic_segment(seg));
    }
    p
}

/// App-Router directory (between `app/` and `/route.<ext>`), if `norm` is one.
fn app_router_dir(norm: &str) -> Option<String> {
    let stem = module_path(norm);
    let base = stem.strip_suffix("/route")?;
    if base == "app" || base.ends_with("/app") {
        return Some(String::new());
    }
    let after = if let Some(i) = base.find("/app/") {
        &base[i + "/app/".len()..]
    } else {
        base.strip_prefix("app/")?
    };
    Some(after.to_string())
}

/// App-Router directory → HTTP path (drops `(groups)` and `@slots`; `[id]` → `:id`).
fn next_app_router_path(dir: &str) -> String {
    let mut segs = Vec::new();
    for seg in dir.split('/').filter(|s| !s.is_empty()) {
        if (seg.starts_with('(') && seg.ends_with(')')) || seg.starts_with('@') {
            continue;
        }
        segs.push(next_dynamic_segment(seg));
    }
    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

/// Remix route file (after `app/routes/`), if `norm` is one.
fn remix_route_file(norm: &str) -> Option<&str> {
    let idx = norm.find("app/routes/")?;
    if idx != 0 && norm.as_bytes()[idx - 1] != b'/' {
        return None;
    }
    Some(&norm[idx + "app/routes/".len()..])
}

/// Remix route filename → HTTP path (`users.$id.tsx` → `/users/:id`; `$` splat → `*`).
fn remix_route_path(routefile: &str) -> String {
    let stem = module_path(routefile);
    let stem = stem.strip_suffix("/route").unwrap_or(&stem);
    let mut segs = Vec::new();
    for seg in stem.split(['/', '.']) {
        if seg.is_empty() || seg == "_index" || seg.starts_with('_') {
            continue;
        }
        segs.push(match seg.strip_prefix('$') {
            Some("") => "*".to_string(),          // bare `$` splat
            Some(name) => format!(":{name}"),      // `$id` → `:id`
            None => seg.to_string(),
        });
    }
    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

/// Detect file-based routes from the path convention + exported handler names:
/// Next.js pages API (all-methods handler), App Router (`export GET/POST/…`),
/// and Remix (`loader` → GET, `action` → POST).
fn detect_file_based_routes(rel: &str, root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let norm = rel.strip_prefix("src/").unwrap_or(rel);

    if let Some(rest) = pages_api_subpath(norm) {
        let path = next_pages_api_path(rest);
        builder.emit_backend_route(root, RouteSource::NextJs, "ALL", &path);
        return;
    }
    if let Some(dir) = app_router_dir(norm) {
        let path = next_app_router_path(&dir);
        let exports = exported_top_level_names(root, src);
        for verb in [
            "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS",
        ] {
            if exports.contains(verb) {
                builder.emit_backend_route(root, RouteSource::NextJs, verb, &path);
            }
        }
        return;
    }
    if let Some(routefile) = remix_route_file(norm) {
        let exports = exported_top_level_names(root, src);
        if exports.contains("loader") || exports.contains("action") {
            let path = remix_route_path(routefile);
            if exports.contains("loader") {
                builder.emit_backend_route(root, RouteSource::Remix, "GET", &path);
            }
            if exports.contains("action") {
                builder.emit_backend_route(root, RouteSource::Remix, "POST", &path);
            }
        }
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
    contract_sites: Vec<ContractSite>,
    string_constants: Vec<StringConstant>,
    http_wrappers: Vec<HttpWrapperDef>,
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
            props: None,
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
            framework_role: stereotype.map(|s| s.to_string()),
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
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
            props: None,
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
            framework_role: None,
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
        });
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

        if let Some(ref owner_id) = owner_id {
            self.edges.push(Edge {
                src: owner_id.clone(),
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
            .and_then(|b| compute_body_fingerprint(b, "typescript", normalize_leaf_token_typescript));
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
            props: None,
        });
    }

    /// Emit a `Route` node for a call/config-based backend framework
    /// (Express/Fastify/Koa/Hapi). No handler edge — the handler is an inline
    /// callback we don't resolve here (parity with the original Express path).
    fn emit_backend_route(
        &mut self,
        call_node: TsNode<'_>,
        source: RouteSource,
        http_method: &str,
        path: &str,
    ) {
        let label = route_source_label(source);
        let route_id = NodeId::new(format!("Route:{label}:{http_method}:{path}"));
        let name = format!("{http_method} {path}");
        self.nodes.push(Node {
            id: route_id,
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: self.rel.clone(),
            range: range_of(call_node),
            props: Some(serde_json::json!({
                "httpMethod": http_method,
                "path": path,
                "route_annotations": [],
                "source": source,
            })),
        });
    }

    /// Emit a non-HTTP `ContractSite` (`ContractKind::Custom(tag)`) — used for
    /// GraphQL resolvers (`tag="graphql"`) and tRPC procedures (`tag="trpc"`).
    /// Custom contracts are visible/queryable but not auto-resolved to edges.
    fn emit_custom_contract(
        &mut self,
        tag: &str,
        op: &str,
        name: Option<String>,
        in_callable: NodeId,
        range: Range,
    ) {
        self.contract_sites.push(ContractSite {
            kind: ContractKind::Custom(tag.to_string()),
            url_template: name,
            topic: None,
            http_method: Some(op.to_string()),
            messaging_framework: None,
            url_parts: None,
            via_wrapper: None,
            in_callable,
            range,
        });
    }

    /// True if any (non-static) import's module path equals or starts with `pkg`
    /// (so `@koa/router` matches `@koa/router`, `koa` matches `koa`).
    fn imports_pkg(&self, pkg: &str) -> bool {
        self.imports.iter().any(|imp| {
            !imp.is_static && (imp.raw == pkg || imp.raw.starts_with(&format!("{pkg}/")))
        })
    }

    /// Pick the backend framework for a verb call `<object>.<verb>(...)`, using
    /// the receiver name disambiguated by the file's imports. Express stays the
    /// default for `app`/`router`/`express` so existing behavior is preserved.
    fn route_framework_for(&self, object: &str) -> Option<RouteSource> {
        let has_express = self.imports_pkg("express");
        let has_fastify = self.imports_pkg("fastify");
        let has_koa = self.imports_pkg("koa") || self.imports_pkg("@koa/router");
        match object {
            "fastify" => Some(RouteSource::Fastify),
            // `const app = fastify()` — attribute to Fastify only when the file
            // imports fastify and not express (an express app also uses `app`).
            "app" if has_fastify && !has_express => Some(RouteSource::Fastify),
            "router" if has_koa && !has_express => Some(RouteSource::Koa),
            "app" | "router" | "express" => Some(RouteSource::Express),
            _ => None,
        }
    }

    fn emit_import(&mut self, node: TsNode<'_>, src: &str) {
        // import_statement → `from` "path" + named/namespace/default imports.
        // The module path becomes the raw import; a namespace import's local
        // binding (`import * as api from './m'`) is recorded as the alias —
        // named and default bindings are not captured.
        let mut from_path = None;
        let mut alias = None;
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "string" => from_path = Some(unquote(&text(child, src))),
                "import_clause" => {
                    let mut clause_cursor = child.walk();
                    for clause_child in child.named_children(&mut clause_cursor) {
                        if clause_child.kind() == "namespace_import" {
                            let mut ns_cursor = clause_child.walk();
                            alias = clause_child
                                .named_children(&mut ns_cursor)
                                .find(|inner| inner.kind() == "identifier")
                                .map(|inner| text(inner, src));
                        }
                    }
                }
                _ => {}
            }
        }
        let raw = from_path.unwrap_or_else(|| text(node, src));
        self.imports.push(RawImport {
            raw,
            is_static: false,
            is_wildcard: false,
            alias,
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
            arg_texts: Vec::new(),
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

// ── Outbound HTTP contract sites (fetch / axios) ──────────────────────────────
//
// Tight recognizers to avoid false positives: bare `fetch(url[, {method}])`
// (default GET), `axios.<verb>(url, …)`, and `axios(url, {method})`. Instance
// clients (`this.http.get(...)`) are out of scope v1. URLs reuse the Phase B
// parts model: template-string substitutions become `Dynamic` parts and fold
// to `{*}` at resolve.

fn axios_http_verb(prop: &str) -> Option<&'static str> {
    match prop {
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
    enclosing_fn: Option<&NodeId>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    let mut via_wrapper = None;
    let http_method = match func.kind() {
        "identifier" => match text(func, src).as_str() {
            // Method comes from the second-arg options object, default GET.
            "fetch" | "axios" => call_options_method(node, src).unwrap_or_else(|| "GET".into()),
            // Any other plain identifier MAY be a same-repo HTTP wrapper
            // (`apiFetch('/admin/x', { method: 'POST' }, token)`). Emit a
            // PROVISIONAL site only when arg 0 is URL-ish; the resolve phase
            // joins it against detected wrapper defs and drops non-matches.
            callee => {
                let Some(arg0) = ts_positional_argument(node, 0) else {
                    return;
                };
                if !ts_arg_is_url_ish(arg0, src) {
                    return;
                }
                via_wrapper = Some(callee.to_string());
                call_options_method(node, src).unwrap_or_else(|| "GET".into())
            }
        },
        "member_expression" => {
            let object_node = func.child_by_field_name("object");
            let object = object_node.map(|n| text(n, src));
            if object.as_deref() == Some("axios") {
                let Some(verb) = func
                    .child_by_field_name("property")
                    .and_then(|prop| axios_http_verb(&text(prop, src)))
                else {
                    return;
                };
                verb.to_string()
            } else {
                // Namespace-import alias receiver (`import * as api from
                // './apiClient'; api.apiFetch('/x')`). Bare identifiers
                // matching a known import alias only — `this.http.get` has a
                // member-expression receiver and `myobj.get` matches no
                // import, so instance clients never emit.
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
                if !ts_arg_is_url_ish(arg0, src) {
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
        fold_ts_url_expr(url_node, src, &mut parts);
        if parts.is_empty() {
            return;
        }
        (None, Some(parts))
    } else {
        let template = literal_ts_string(url_node, src).map(|url| normalize_external_url(&url));
        let parts = if template.is_none() {
            ts_url_parts(url_node, src)
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
fn try_emit_trpc_contract(
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
    let in_callable = enclosing_fn
        .cloned()
        .unwrap_or_else(|| file_id(&builder.rel));
    builder.emit_custom_contract("trpc", op, None, in_callable, range_of(node));
}

/// URL-ish gate for provisional wrapper calls: a string starting with `/`, a
/// template whose first fragment starts with `/`, or a concat whose folded
/// first part is such a Lit. Keeps `t('common.title')` / `helper(id)` out.
fn ts_arg_is_url_ish(node: TsNode<'_>, src: &str) -> bool {
    let mut parts = Vec::new();
    fold_ts_url_expr(node, src, &mut parts);
    matches!(parts.first(), Some(UrlPart::Lit(lit)) if lit.starts_with('/'))
}

fn ts_positional_argument(call: TsNode<'_>, n: usize) -> Option<TsNode<'_>> {
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

/// `method: 'POST'` from a call's second-argument options object literal.
fn call_options_method(call: TsNode<'_>, src: &str) -> Option<String> {
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

/// Text of a plain string literal (`'…'` / `"…"`) — template strings and
/// expressions are not literals.
fn literal_ts_string(node: TsNode<'_>, src: &str) -> Option<String> {
    (node.kind() == "string").then(|| unquote(&text(node, src)))
}

/// Phase B parts for a non-literal URL argument: template-string fragments →
/// `Lit`, a `${IDENT}` substitution → `ConstRef` (resolved cross-file via
/// module constants and the gated unique-name fallback), any other `${…}` →
/// `Dynamic`, `+`-concat folds recursively. Unresolved refs degrade to `{*}`
/// — never a wrong match.
fn ts_url_parts(node: TsNode<'_>, src: &str) -> Option<Vec<UrlPart>> {
    let mut parts = Vec::new();
    fold_ts_url_expr(node, src, &mut parts);
    parts
        .iter()
        .any(|part| !matches!(part, UrlPart::Lit(_)))
        .then_some(parts)
}

fn fold_ts_url_expr(node: TsNode<'_>, src: &str, out: &mut Vec<UrlPart>) {
    match node.kind() {
        "string" => out.push(UrlPart::Lit(unquote(&text(node, src)))),
        "template_string" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_fragment" | "escape_sequence" => out.push(UrlPart::Lit(
                        child.utf8_text(src.as_bytes()).unwrap_or_default().to_string(),
                    )),
                    // `${API_BASE_URL}` → ConstRef; SCREAMING_SNAKE identifiers
                    // only — params/locals (`${id}`) and property chains
                    // (`${cfg.base}`) stay Dynamic so they can never feed the
                    // cross-file unique-name fallback.
                    "template_substitution" => match child.named_child(0) {
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
                Some(left) => fold_ts_url_expr(left, src, out),
                None => out.push(UrlPart::Dynamic),
            }
            match node.child_by_field_name("right") {
                Some(right) => fold_ts_url_expr(right, src, out),
                None => out.push(UrlPart::Dynamic),
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => fold_ts_url_expr(inner, src, out),
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
fn collect_module_string_constants(node: TsNode<'_>, src: &str, builder: &mut Builder) {
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

/// One piece of a candidate wrapper's URL expression: a regular part, or the
/// pass-through parameter slot.
enum WrapperUrlPiece {
    Part(UrlPart),
    Param,
}

/// Detect a same-repo HTTP wrapper: a module-scope function whose FIRST param
/// is a plain identifier and whose body calls fetch/axios with a URL that is
/// `<Lit/ConstRef prefix…><param>` (param LAST) — directly or via one level of
/// `const url = <expr>` same-body indirection. Anything fancier bails: a
/// missed wrapper degrades coverage, a wrong one would fabricate endpoints.
fn try_collect_http_wrapper(name: &str, fn_node: TsNode<'_>, src: &str, builder: &mut Builder) {
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
fn first_param_identifier(fn_node: TsNode<'_>, src: &str) -> Option<String> {
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
fn find_inner_http_call<'a>(body: TsNode<'a>, src: &str) -> Option<TsNode<'a>> {
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
fn find_unique_const_initializer<'a>(
    body: TsNode<'a>,
    local: &str,
    src: &str,
) -> Option<TsNode<'a>> {
    let mut found: Option<TsNode<'a>> = None;
    collect_const_initializers(body, local, src, &mut found, &mut 0);
    found
}

fn collect_const_initializers<'a>(
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
fn fold_wrapper_url_expr(
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

// ── Recursive AST walker ──────────────────────────────────────────────────────

/// `enclosing_fn` is the function/method that lexically contains `node`, or
/// `None` at module / class-body scope — contract sites are attributed to it
/// and fall back to the file id (which degrades cross-repo trace entry
/// resolution; pinned by test).
fn walk(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    class_fqn: Option<&str>,
    controller_prefix: Option<&str>,
    enclosing_fn: Option<&NodeId>,
) {
    match node.kind() {
        "program" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, None, None, None);
            }
        }
        "export_statement" => {
            // export default class / export function / export const ...
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
            }
        }
        "lexical_declaration" => {
            // Module-level `const X = '…'` (incl. env-default initializers)
            // becomes a StringConstant for cross-file URL folding. Recurse
            // regardless — initializers can contain contract calls.
            if class_fqn.is_none() && enclosing_fn.is_none() {
                collect_module_string_constants(node, src, builder);
                // `export const apiFetch = async (endpoint, …) => …` — the
                // dominant wrapper shape.
                let mut cursor = node.walk();
                for declarator in node.named_children(&mut cursor) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let (Some(name_node), Some(value)) = (
                        declarator.child_by_field_name("name"),
                        declarator.child_by_field_name("value"),
                    ) else {
                        continue;
                    };
                    if name_node.kind() == "identifier" && value.kind() == "arrow_function" {
                        try_collect_http_wrapper(&text(name_node, src), value, src, builder);
                    }
                }
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
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
                    walk(child, src, builder, Some(&fqn), Some(&ctrl_prefix), None);
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
            if class_fqn.is_none() {
                try_collect_http_wrapper(&name, node, src, builder);
            }
            let fn_id = builder.emit_function(node, src, &name, arity, class_fqn);

            // Check NestJS decorators
            let ctrl_prefix = controller_prefix.unwrap_or("");
            for (dname, dpath) in &decorators {
                if let Some(http_method) = nestjs_http_method(dname) {
                    let method_path = dpath.as_deref().unwrap_or("");
                    let full_path = join_paths(ctrl_prefix, method_path);
                    builder.emit_nestjs_route(node, &fn_id, http_method, &full_path, dname);
                }
                if let Some(op) = graphql_operation(dname) {
                    let opname = dpath.clone().unwrap_or_else(|| name.clone());
                    builder.emit_custom_contract(
                        "graphql",
                        op,
                        Some(opname),
                        fn_id.clone(),
                        range_of(node),
                    );
                }
            }

            // Walk body for call references
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, class_fqn, controller_prefix, Some(&fn_id));
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
                if let Some(op) = graphql_operation(dname) {
                    let opname = dpath.clone().unwrap_or_else(|| name.clone());
                    builder.emit_custom_contract(
                        "graphql",
                        op,
                        Some(opname),
                        fn_id.clone(),
                        range_of(node),
                    );
                }
            }

            // Walk body
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    walk(child, src, builder, class_fqn, controller_prefix, Some(&fn_id));
                }
            }
        }
        "import_statement" => {
            builder.emit_import(node, src);
        }
        "call_expression" => {
            // Backend HTTP routes: Express / Fastify / Koa verb calls + Fastify/Hapi
            // config-object `.route({...})` (import-gated; Express behavior unchanged).
            detect_call_route(node, src, builder);
            try_emit_http_contract(node, src, builder, enclosing_fn);
            try_emit_trpc_contract(node, src, builder, enclosing_fn);
            builder.emit_call_reference(node, src);
            // recurse into arguments
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
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
                http_wrappers: Vec::new(),
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

    walk(tree.root_node(), src, &mut builder, None, None, None);

    // File-based routes (Next.js / Remix) are a path convention, not a call —
    // detect after the walk so exported handler names are available.
    detect_file_based_routes(rel, tree.root_node(), src, &mut builder);

    // Convert RawImports to ImportBindings (best-effort for TypeScript)
    let import_bindings = builder.imports.iter().map(|imp| {
        use cih_core::{ImportBinding, ImportBindingKind};
        ImportBinding {
            module: imp.raw.clone(),
            imported: None,
            local: None,
            kind: ImportBindingKind::Named,
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
            contract_sites: builder.contract_sites,
            sql_constants: Vec::new(),
            sql_execution_sites: Vec::new(),
            string_constants: builder.string_constants,
            http_wrappers: builder.http_wrappers,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{module_path, parse_typescript_file};

    #[test]
    fn parses_javascript_file() {
        // JS is handled by the TypeScript provider: functions + Express routes
        // are extracted the same as in .ts files.
        let src = r#"const express = require('express');
const app = express();
async function getStock(id) {
    const r = await fetch(`http://inventory/api/stock/${id}`);
    return r.json();
}
app.get('/api/orders/:id', async (req, res) => {
    res.json(await getStock(req.params.id));
});
module.exports = app;
"#;
        let unit = parse_typescript_file("src/server.js", src).expect("JS parses");
        let names: Vec<&str> = unit.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.contains(&"getStock"),
            "getStock function node missing: {names:?}"
        );
        assert!(
            unit.nodes.iter().any(|n| {
                let id = n.id.as_str();
                id.starts_with("Route:express:GET") && id.contains("orders")
            }),
            "express GET /api/orders route node missing: {:?}",
            unit.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn module_path_strips_js_and_ts_extensions() {
        for (input, want) in [
            ("src/a.mjs", "src/a"),
            ("src/a.cjs", "src/a"),
            ("src/a.jsx", "src/a"),
            ("src/a.js", "src/a"),
            ("src/a.tsx", "src/a"),
            ("src/a.ts", "src/a"),
            ("src/a.min.js", "src/a.min"),
        ] {
            assert_eq!(module_path(input), want, "module_path({input})");
        }
    }

    // ── P1: additional backend route frameworks ──────────────────────────────

    fn route_ids(unit: &cih_core::ParsedUnit) -> Vec<String> {
        unit.nodes
            .iter()
            .filter(|n| n.kind == cih_core::NodeKind::Route)
            .map(|n| n.id.as_str().to_string())
            .collect()
    }

    fn has_route(unit: &cih_core::ParsedUnit, id_contains: &str) -> bool {
        route_ids(unit).iter().any(|id| id.contains(id_contains))
    }

    #[test]
    fn fastify_verb_and_config_routes() {
        let src = r#"import fastify from 'fastify';
const app = fastify();
app.get('/api/users/:id', async () => ({}));
app.route({ method: ['GET', 'POST'], url: '/api/items' });
"#;
        let unit = parse_typescript_file("src/app.ts", src).expect("parses");
        let ids = route_ids(&unit);
        assert!(
            ids.iter().any(|i| i == "Route:fastify:GET:/api/users/:id"),
            "fastify verb route missing: {ids:?}"
        );
        assert!(
            has_route(&unit, "Route:fastify:GET:/api/items")
                && has_route(&unit, "Route:fastify:POST:/api/items"),
            "fastify config routes missing: {ids:?}"
        );
    }

    #[test]
    fn koa_router_import_gated() {
        let src = r#"import Router from '@koa/router';
const router = new Router();
router.get('/api/ping', async (ctx) => { ctx.body = 'ok'; });
"#;
        let unit = parse_typescript_file("src/routes.ts", src).expect("parses");
        assert!(
            has_route(&unit, "Route:koa:GET:/api/ping"),
            "koa route missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn hapi_config_route() {
        let src = r#"import Hapi from '@hapi/hapi';
const server = Hapi.server({ port: 3000 });
server.route({ method: 'GET', path: '/api/health', handler: () => 'ok' });
"#;
        let unit = parse_typescript_file("src/server.ts", src).expect("parses");
        assert!(
            has_route(&unit, "Route:hapi:GET:/api/health"),
            "hapi route missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn express_unchanged_when_no_fastify_import() {
        // `router` without a koa import, `app` without a fastify import → Express.
        let src = r#"import express from 'express';
const app = express();
app.post('/api/orders', (req, res) => res.end());
"#;
        let unit = parse_typescript_file("src/index.ts", src).expect("parses");
        assert!(
            has_route(&unit, "Route:express:POST:/api/orders"),
            "express route missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn nextjs_pages_api_route() {
        let src = "export default function handler(req, res) { res.json({}); }";
        let unit =
            parse_typescript_file("src/pages/api/users/[id].ts", src).expect("parses");
        assert!(
            has_route(&unit, "Route:nextjs:ALL:/api/users/:id"),
            "next pages api route missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn nextjs_app_router_route() {
        let src = r#"export async function GET() { return Response.json({}); }
export async function POST() { return Response.json({}); }
"#;
        let unit =
            parse_typescript_file("app/orders/[id]/route.ts", src).expect("parses");
        assert!(
            has_route(&unit, "Route:nextjs:GET:/orders/:id")
                && has_route(&unit, "Route:nextjs:POST:/orders/:id"),
            "next app router routes missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn remix_loader_action_routes() {
        let src = r#"export async function loader() { return {}; }
export async function action() { return {}; }
"#;
        let unit =
            parse_typescript_file("app/routes/users.$id.tsx", src).expect("parses");
        assert!(
            has_route(&unit, "Route:remix:GET:/users/:id")
                && has_route(&unit, "Route:remix:POST:/users/:id"),
            "remix routes missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn graphql_resolver_custom_contract() {
        let src = r#"import { Resolver, Query, Mutation } from 'type-graphql';
@Resolver()
class UserResolver {
    @Query()
    users() { return []; }
    @Mutation()
    createUser() { return {}; }
}
"#;
        let unit = parse_typescript_file("src/user.resolver.ts", src).expect("parses");
        let gql: Vec<_> = unit
            .parsed_file
            .contract_sites
            .iter()
            .filter(|c| matches!(&c.kind, cih_core::ContractKind::Custom(t) if t == "graphql"))
            .collect();
        assert_eq!(gql.len(), 2, "expected 2 graphql contract sites");
        assert!(gql.iter().any(|c| c.http_method.as_deref() == Some("QUERY")));
        assert!(gql
            .iter()
            .any(|c| c.http_method.as_deref() == Some("MUTATION")));
    }

    #[test]
    fn trpc_procedure_custom_contract() {
        let src = r#"import { initTRPC } from '@trpc/server';
const t = initTRPC.create();
export const appRouter = t.router({
    getUser: t.procedure.query(() => ({ id: 1 })),
    setUser: t.procedure.mutation(() => ({ ok: true })),
});
"#;
        let unit = parse_typescript_file("src/router.ts", src).expect("parses");
        let trpc: Vec<_> = unit
            .parsed_file
            .contract_sites
            .iter()
            .filter(|c| matches!(&c.kind, cih_core::ContractKind::Custom(t) if t == "trpc"))
            .collect();
        assert!(
            trpc.iter().any(|c| c.http_method.as_deref() == Some("QUERY"))
                && trpc
                    .iter()
                    .any(|c| c.http_method.as_deref() == Some("MUTATION")),
            "trpc query+mutation contracts missing: {} sites",
            trpc.len()
        );
    }
}
