use cih_core::{
    db_query_inline_id, db_table_id, field_id, file_id, function_id, type_id, BindingKind,
    ContractKind,
    ContractSite, Edge, EdgeKind, MessagingFramework, Node, NodeId, NodeKind, ParsedFile,
    ParsedUnit, Range, RawImport, RefKind, ReferenceSite, RouteSource, HttpWrapperDef,
    StringConstant, SymbolDef, TypeBinding, UrlPart,
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

/// Collect the decorators that decorate `node`, handling both grammar shapes:
/// (a) leading `decorator` **children** of the node (top-level `class_declaration`),
/// and (b) the contiguous run of `decorator` **siblings** immediately preceding it
/// (`method_definition` / `function_declaration` in a class/statement body).
///
/// The sibling run resets on any non-decorator sibling — without it, later members
/// inherit earlier members' decorators (duplicate routes / contracts).
fn collect_sibling_decorators<'a>(node: TsNode<'a>, src: &str) -> Vec<(String, Option<String>)> {
    // (a) Leading decorator children of the node itself.
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "decorator" => {
                if let Some(info) = decorator_info(child, src) {
                    out.push(info);
                }
            }
            "comment" => {}
            _ => break, // first non-decorator child (the `class`/`function` keyword)
        }
    }
    if !out.is_empty() {
        return out;
    }

    // (b) Preceding decorator siblings of the node. `@Dec() export class X` nests
    // the class under an `export_statement` whose children are
    // `[decorator, "export", class_declaration]` — the reset must ignore the
    // `export`/`abstract`/`{` keyword & punctuation tokens between them.
    preceding_decorators(node, src)
}

/// The contiguous run of `decorator` siblings immediately preceding `node`,
/// resetting only on a *named* non-decorator sibling (a real member/statement),
/// so members don't inherit each other's decorators while keyword/punctuation
/// tokens (`export`, `abstract`, `{`) between a decorator and its target are ignored.
fn preceding_decorators(node: TsNode<'_>, src: &str) -> Vec<(String, Option<String>)> {
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
            "comment" => {}
            _ if !child.is_named() => {} // keyword / punctuation token — not a boundary
            _ => out.clear(), // a real declaration/statement ends the run
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
        RouteSource::GraphQl => "graphql",
        RouteSource::Trpc => "trpc",
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

// ── DB / ORM access (Prisma / Mongoose / Sequelize / Knex / TypeORM) ──────────

/// Classify an ORM method name as a DB op: `Some(is_write)`, or `None` if the
/// method is not a recognized data-access operation.
fn db_op_kind(op: &str) -> Option<bool> {
    match op {
        "find" | "findOne" | "findById" | "findByPk" | "findAll" | "findMany"
        | "findUnique" | "findFirst" | "findUniqueOrThrow" | "findFirstOrThrow" | "count"
        | "aggregate" | "groupBy" | "exists" | "distinct"
        // Knex query-builder terminals (read).
        | "select" | "first" | "pluck" => Some(false),
        "create" | "createMany" | "save" | "insert" | "insertMany" | "bulkCreate" | "update"
        | "updateOne" | "updateMany" | "upsert" | "delete" | "deleteOne" | "deleteMany"
        | "destroy" | "remove" | "findOneAndUpdate" | "findOneAndDelete"
        | "findByIdAndUpdate" | "findByIdAndDelete"
        // Knex write terminals.
        | "del" | "increment" | "decrement" => Some(true),
        _ => None,
    }
}

/// A model-defining call → `(table_name, engine)`: `mongoose.model('T',…)`,
/// bare `model('T',…)` (mongoose named import), or `sequelize.define('T',…)`.
fn db_model_definition(value: TsNode<'_>, src: &str) -> Option<(String, &'static str)> {
    if value.kind() != "call_expression" {
        return None;
    }
    let func = value.child_by_field_name("function")?;
    let engine = match func.kind() {
        "identifier" if text(func, src) == "model" => "mongoose",
        "member_expression" => {
            let obj = func
                .child_by_field_name("object")
                .map(|n| text(n, src))
                .unwrap_or_default();
            let prop = func
                .child_by_field_name("property")
                .map(|n| text(n, src))
                .unwrap_or_default();
            if prop == "define" {
                "sequelize"
            } else if obj == "mongoose" && prop == "model" {
                "mongoose"
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let table = first_string_arg_in_call(value, src)?;
    Some((table, engine))
}

/// Pre-pass: record ORM model vars (`const User = mongoose.model('User',…)`) →
/// table name, and emit the `DbTable` node.
fn collect_db_models(root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let rel = builder.rel.clone();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "variable_declarator" {
            if let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) {
                if name.kind() == "identifier" {
                    if let Some((table, _engine)) = db_model_definition(value, src) {
                        builder.db_models.insert(text(name, src), table.clone());
                        builder.emit_db_table(&table, &rel, range_of(n));
                    }
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Receiver base name for a Prisma call `<base>.<model>.<op>()`: `prisma` /
/// `this.prisma`.
fn prisma_base_name(base: TsNode<'_>, src: &str) -> Option<String> {
    match base.kind() {
        "identifier" => Some(text(base, src)),
        "member_expression" => {
            let inner = base.child_by_field_name("object")?;
            if text(inner, src) == "this" {
                base.child_by_field_name("property").map(|p| text(p, src))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Unwind a Knex receiver chain (`knex('t').where(…).…`) to the root
/// `knex('t')` call and return the table literal.
fn knex_root_table(mut recv: TsNode<'_>, src: &str, builder: &Builder) -> Option<String> {
    loop {
        match recv.kind() {
            "call_expression" => {
                let f = recv.child_by_field_name("function")?;
                if f.kind() == "identifier" {
                    let name = text(f, src);
                    if name == "knex" || (name == "db" && builder.imports_pkg("knex")) {
                        return first_string_arg_in_call(recv, src);
                    }
                    return None;
                }
                // Member call (`.where(…)`) — descend into its receiver.
                recv = f.child_by_field_name("object")?;
            }
            "member_expression" => recv = recv.child_by_field_name("object")?,
            _ => return None,
        }
    }
}

/// Detect an ORM data-access call and emit `DbQuery`/`DbTable` + edges: Prisma
/// (`prisma.model.op`), Mongoose/Sequelize model methods, and Knex query builders.
fn try_emit_db_query(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "member_expression" {
        return;
    }
    let Some(prop_node) = func.child_by_field_name("property") else {
        return;
    };
    let op = text(prop_node, src);
    let Some(is_write) = db_op_kind(&op) else {
        return;
    };
    let Some(object) = func.child_by_field_name("object") else {
        return;
    };
    let in_callable = enclosing_fn
        .cloned()
        .unwrap_or_else(|| file_id(&builder.rel));

    // Prisma: `prisma.<model>.<op>()` — object is the `prisma.<model>` member.
    if object.kind() == "member_expression" {
        if let (Some(base), Some(model)) = (
            object.child_by_field_name("object"),
            object.child_by_field_name("property"),
        ) {
            if let Some(bn) = prisma_base_name(base, src) {
                let gated = bn == "prisma"
                    || (builder.imports_pkg("@prisma/client") && matches!(bn.as_str(), "db"));
                if gated {
                    let table = text(model, src);
                    builder.emit_db_query(node, &table, &op, "prisma", is_write, &in_callable);
                    return;
                }
            }
        }
    }

    // Mongoose/Sequelize model var: `User.find()`.
    if object.kind() == "identifier" {
        if let Some(table) = builder.db_models.get(&text(object, src)).cloned() {
            builder.emit_db_query(node, &table, &op, "orm", is_write, &in_callable);
            return;
        }
    }

    // Knex query builder: `knex('t').where(…).select()`.
    if let Some(table) = knex_root_table(object, src, builder) {
        builder.emit_db_query(node, &table, &op, "knex", is_write, &in_callable);
    }
}

// ── Messaging / realtime (P5) ─────────────────────────────────────────────────

/// True if `value` is `new Queue('name')` (Bull/BullMQ).
fn is_new_queue(value: TsNode<'_>, src: &str) -> bool {
    value.kind() == "new_expression"
        && value
            .child_by_field_name("constructor")
            .map(|c| text(c, src))
            .as_deref()
            == Some("Queue")
}

/// Pre-pass: record `const q = new Queue('emails')` vars → queue name.
fn collect_queue_instances(root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "variable_declarator" {
            if let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) {
                if name.kind() == "identifier" && is_new_queue(value, src) {
                    if let Some(q) = first_string_arg_in_call(value, src) {
                        builder.queue_instances.insert(text(name, src), q);
                    }
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Topic literal from a kafkajs `{ topic: 't', … }` first-arg config.
fn kafka_topic_arg(node: TsNode<'_>, src: &str) -> Option<String> {
    let arg0 = ts_positional_argument(node, 0)?;
    if arg0.kind() != "object" {
        return None;
    }
    literal_ts_string(object_pair_value(arg0, "topic", src)?, src)
}

/// Detect a messaging call and emit an `EventPublish`/`EventListen` contract:
/// socket.io, kafkajs, Bull/BullMQ, amqplib (all import-gated to bound false
/// positives on the generic method names `emit`/`on`/`send`/`add`).
fn try_emit_messaging(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
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
    let obj_text = text(obj, src);
    let prop = text(prop_node, src);
    let in_callable = || {
        enclosing_fn
            .cloned()
            .unwrap_or_else(|| file_id(&builder.rel))
    };

    // socket.io realtime events.
    if builder.imports_pkg("socket.io") || builder.imports_pkg("socket.io-client") {
        let publish = match prop.as_str() {
            "emit" => Some(true),
            "on" => Some(false),
            _ => None,
        };
        if let Some(is_pub) = publish {
            if let Some(topic) = first_string_arg_in_call(node, src) {
                builder.emit_event_contract(
                    node,
                    topic,
                    MessagingFramework::SocketIo,
                    is_pub,
                    in_callable(),
                );
                return;
            }
        }
    }

    // kafkajs producer/consumer.
    if builder.imports_pkg("kafkajs") {
        let publish = match prop.as_str() {
            "send" => Some(true),
            "subscribe" => Some(false),
            _ => None,
        };
        if let Some(is_pub) = publish {
            if let Some(topic) = kafka_topic_arg(node, src) {
                builder.emit_event_contract(
                    node,
                    topic,
                    MessagingFramework::Kafka,
                    is_pub,
                    in_callable(),
                );
                return;
            }
        }
    }

    // Bull/BullMQ: `queue.add(...)` publishes to the tracked queue name.
    if (builder.imports_pkg("bull") || builder.imports_pkg("bullmq")) && prop == "add" {
        if let Some(topic) = builder.queue_instances.get(&obj_text).cloned() {
            builder.emit_event_contract(
                node,
                topic,
                MessagingFramework::Bull,
                true,
                in_callable(),
            );
            return;
        }
    }

    // amqplib (RabbitMQ) channel ops.
    if builder.imports_pkg("amqplib") {
        let publish = match prop.as_str() {
            "sendToQueue" | "publish" => Some(true),
            "consume" => Some(false),
            _ => None,
        };
        if let Some(is_pub) = publish {
            if let Some(topic) = first_string_arg_in_call(node, src) {
                builder.emit_event_contract(
                    node,
                    topic,
                    MessagingFramework::Rabbitmq,
                    is_pub,
                    in_callable(),
                );
            }
        }
    }
}

// ── Component stereotypes + DI (P4) ───────────────────────────────────────────

/// True if the class extends `React.Component` / `Component` / `PureComponent`.
fn class_extends_react_component(node: TsNode<'_>, src: &str) -> bool {
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if child.kind() == "class_heritage" {
            return text(child, src).contains("Component");
        }
    }
    false
}

/// Stereotype for a top-level function: React component (PascalCase) or hook
/// (`use<Upper>`), gated on a `react` import (the grammar can't confirm JSX).
fn react_function_stereotype(name: &str, builder: &Builder) -> Option<String> {
    if !builder.imports_pkg("react") {
        return None;
    }
    let rest = name.strip_prefix("use");
    if matches!(rest.and_then(|r| r.chars().next()), Some(c) if c.is_uppercase()) {
        return Some("react_hook".to_string());
    }
    if name.chars().next().is_some_and(|c| c.is_uppercase()) {
        return Some("react_component".to_string());
    }
    None
}

/// A class stereotype that participates in constructor DI (Nest/Angular provider).
fn is_di_provider(stereotype: Option<&str>) -> bool {
    matches!(
        stereotype,
        Some("nestjs_controller")
            | Some("nestjs_injectable")
            | Some("angular_injectable")
            | Some("angular_component")
            | Some("graphql_resolver")
    )
}

/// Simple type name from a heritage clause value: `A` → A, `React.Component` → B
/// (last property), `Base<T>` → Base. The resolver keys on this name.
fn heritage_type_name(node: TsNode<'_>, src: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(text(node, src)),
        "member_expression" => node.child_by_field_name("property").map(|p| text(p, src)),
        "generic_type" => {
            let mut c = node.walk();
            let base = node
                .named_children(&mut c)
                .find(|n| matches!(n.kind(), "type_identifier" | "identifier" | "member_expression"))
                .and_then(|n| heritage_type_name(n, src));
            base
        }
        _ => None,
    }
}

/// Simple type name from a `type_annotation` node (`: User` → `User`,
/// `: Repository<User>` → `Repository`); `None` for primitives/unions/etc.
fn type_annotation_name(annotation: TsNode<'_>, src: &str) -> Option<String> {
    let mut c = annotation.walk();
    let ty = annotation.named_children(&mut c).next()?;
    match ty.kind() {
        "type_identifier" => Some(text(ty, src)),
        "generic_type" => {
            let mut c2 = ty.walk();
            let base = ty
                .named_children(&mut c2)
                .find(|n| n.kind() == "type_identifier")
                .map(|n| text(n, src));
            base
        }
        _ => None,
    }
}

/// Simple type name of a constructor parameter's `: Type` annotation
/// (`private svc: UserService` → `UserService`; `Repository<User>` → `Repository`).
fn param_type_name(param: TsNode<'_>, src: &str) -> Option<String> {
    let mut c = param.walk();
    let ann = param
        .named_children(&mut c)
        .find(|n| n.kind() == "type_annotation")?;
    let mut c2 = ann.walk();
    let ty = ann.named_children(&mut c2).next()?;
    match ty.kind() {
        "type_identifier" => Some(text(ty, src)),
        "generic_type" => {
            let mut c3 = ty.walk();
            let base = ty
                .named_children(&mut c3)
                .find(|n| n.kind() == "type_identifier")
                .map(|n| text(n, src));
            base
        }
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
    type_bindings: Vec<TypeBinding>,
    contract_sites: Vec<ContractSite>,
    string_constants: Vec<StringConstant>,
    http_wrappers: Vec<HttpWrapperDef>,
    /// `const api = axios.create({ baseURL })` instances → optional literal
    /// baseURL, so `api.get('/x')` resolves to `<baseURL>/x` (P2 instance clients).
    axios_instances: std::collections::HashMap<String, Option<String>>,
    /// ORM model vars → table name (`const User = mongoose.model('User', …)`,
    /// `sequelize.define('users', …)`) so `User.find()` accesses the right table (P3).
    db_models: std::collections::HashMap<String, String>,
    /// DbTable ids already emitted this file (dedup — one table, many queries).
    seen_db_tables: std::collections::HashSet<String>,
    /// Bull/BullMQ queue vars → queue name (`const q = new Queue('emails')`) so
    /// `q.add(...)` publishes to the right destination (P5).
    queue_instances: std::collections::HashMap<String, String>,
}

impl Builder {
    fn emit_class(
        &mut self,
        node: TsNode<'_>,
        _src: &str,
        class_name: &str,
        stereotype: Option<&str>,
    ) -> String {
        let fqn = format!("{}.{}", self.module, class_name);
        let id = type_id(NodeKind::Class, &fqn);
        let range = range_of(node);

        let stereotype = stereotype.map(str::to_string);

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

    /// Emit `Extends`/`Implements` reference sites for a class/interface's heritage
    /// clauses (`class B extends A implements I, J`, `interface X extends Y`). The
    /// resolver resolves each supertype name (via the import map) and builds the
    /// `supertypes`/`implementors` index that powers inherited-member resolution,
    /// `super`, and MRO. `subtype_id` is the edge source; `subtype_fqn` is `in_fqcn`.
    fn emit_heritage(
        &mut self,
        node: TsNode<'_>,
        src: &str,
        subtype_fqn: &str,
        subtype_id: &NodeId,
    ) {
        let push = |this: &mut Self, ty: TsNode<'_>, kind: RefKind| {
            if let Some(name) = heritage_type_name(ty, src) {
                this.reference_sites.push(ReferenceSite {
                    name,
                    receiver: None,
                    kind,
                    arity: None,
                    range: range_of(ty),
                    in_fqcn: subtype_fqn.to_string(),
                    in_callable: subtype_id.clone(),
                    arg_texts: Vec::new(),
                });
            }
        };
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                // Class: `class_heritage` → extends_clause + implements_clause.
                "class_heritage" => {
                    let mut c2 = child.walk();
                    for h in child.named_children(&mut c2) {
                        match h.kind() {
                            "extends_clause" => {
                                if let Some(v) = h.child_by_field_name("value") {
                                    push(self, v, RefKind::Extends);
                                }
                            }
                            "implements_clause" => {
                                let mut c3 = h.walk();
                                for t in h.named_children(&mut c3) {
                                    push(self, t, RefKind::Implements);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // Interface: `extends_type_clause` → one or more `type` fields.
                "extends_type_clause" => {
                    let mut c2 = child.walk();
                    for t in child.named_children(&mut c2) {
                        push(self, t, RefKind::Extends);
                    }
                }
                _ => {}
            }
        }
    }

    /// Emit a `Field` node + `HasField` edge + `SymbolDef` (with `declared_type`)
    /// for a typed class field. The resolver's `field_type_in_hierarchy` reads the
    /// def's `declared_type`, so `this.<field>.method()` resolves the receiver.
    fn emit_field(
        &mut self,
        class_fqn: &str,
        class_id: &NodeId,
        name: &str,
        declared_type: String,
        range: Range,
    ) {
        let id = field_id(class_fqn, name);
        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Field,
            name: name.to_string(),
            qualified_name: Some(format!("{class_fqn}#{name}")),
            file: self.rel.clone(),
            range,
            props: None,
        });
        self.edges.push(Edge {
            src: class_id.clone(),
            dst: id.clone(),
            kind: EdgeKind::HasField,
            confidence: 1.0,
            reason: "member".into(),
            props: None,
        });
        self.defs.push(SymbolDef {
            id,
            kind: NodeKind::Field,
            fqcn: class_fqn.to_string(),
            name: name.to_string(),
            owner: Some(class_id.clone()),
            range,
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: Some(declared_type),
            framework_role: None,
            complexity: None,
            body_fingerprint: None,
            lang_meta: None,
        });
    }

    /// Emit typed fields for a class: `public_field_definition` members with a
    /// type annotation, and constructor **parameter properties**
    /// (`constructor(private repo: Repo)`, detected by an accessibility modifier).
    fn emit_class_fields(
        &mut self,
        class_node: TsNode<'_>,
        src: &str,
        class_fqn: &str,
        class_id: &NodeId,
    ) {
        let Some(body) = class_node.child_by_field_name("body") else {
            return;
        };
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            match member.kind() {
                "public_field_definition" => {
                    let (Some(nm), Some(ty)) = (
                        member.child_by_field_name("name"),
                        member
                            .child_by_field_name("type")
                            .and_then(|a| type_annotation_name(a, src)),
                    ) else {
                        continue;
                    };
                    self.emit_field(class_fqn, class_id, &text(nm, src), ty, range_of(member));
                }
                "method_definition"
                    if member
                        .child_by_field_name("name")
                        .map(|n| text(n, src))
                        .as_deref()
                        == Some("constructor") =>
                {
                    let Some(params) = member.child_by_field_name("parameters") else {
                        continue;
                    };
                    let mut pc = params.walk();
                    for p in params.named_children(&mut pc) {
                        if !matches!(p.kind(), "required_parameter" | "optional_parameter") {
                            continue;
                        }
                        // A parameter property has an accessibility modifier
                        // (`private`/`public`/`protected`) → becomes a field.
                        let mut ic = p.walk();
                        let is_property = p
                            .children(&mut ic)
                            .any(|c| c.kind() == "accessibility_modifier");
                        let (Some(pat), Some(ty)) = (
                            p.child_by_field_name("pattern").filter(|_| is_property),
                            p.child_by_field_name("type")
                                .and_then(|a| type_annotation_name(a, src)),
                        ) else {
                            continue;
                        };
                        if pat.kind() == "identifier" {
                            self.emit_field(class_fqn, class_id, &text(pat, src), ty, range_of(p));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn emit_interface(&mut self, node: TsNode<'_>, src: &str, name: &str) {
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
        self.emit_heritage(node, src, &fqn, &id);
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
        stereotype: Option<&str>,
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
            props: stereotype.map(|s| serde_json::json!({ "stereotype": s })),
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
            framework_role: stereotype.map(str::to_string),
            complexity: None,
            body_fingerprint,
            lang_meta: None,
        });
        // Typed params → type_bindings scoped to this callable's signature.
        let sig = format!("{container_fqn}#{name}/{arity}");
        self.emit_param_bindings(node, src, &sig);
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

    /// Emit a `Route` node for a GraphQL/tRPC producer operation (`path` = the
    /// operation name, `httpMethod` = `QUERY`/`MUTATION`/`SUBSCRIPTION`), plus a
    /// `HandlesRoute` edge from the handler when known. Reuses the Route model so
    /// operations flow through route_map / trace_flow / cross-repo matching.
    fn emit_operation_route(
        &mut self,
        node: TsNode<'_>,
        source: RouteSource,
        method: &str,
        name: &str,
        handler: Option<&NodeId>,
    ) {
        let label = route_source_label(source);
        let route_id = NodeId::new(format!("Route:{label}:{method}:{name}"));
        let display = format!("{method} {name}");
        self.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: display.clone(),
            qualified_name: Some(display),
            file: self.rel.clone(),
            range: range_of(node),
            props: Some(serde_json::json!({
                "httpMethod": method,
                "path": name,
                "route_annotations": [],
                "source": source,
                "operation": true,
            })),
        });
        if let Some(h) = handler {
            self.edges.push(Edge {
                src: h.clone(),
                dst: route_id,
                kind: EdgeKind::HandlesRoute,
                confidence: 1.0,
                reason: format!("{label}-{}", method.to_ascii_lowercase()),
                props: None,
            });
        }
    }

    /// Emit a consumer-side contract for a GraphQL/tRPC operation call. Modeled as
    /// an `HttpCall` (→ `ExternalEndpoint` at resolve) so the cross-repo matcher
    /// links it to the producer `Route` by (method, name). The QUERY/MUTATION/
    /// SUBSCRIPTION method namespace never collides with HTTP GET/POST.
    fn emit_operation_call(&mut self, node: TsNode<'_>, method: &str, name: &str, in_callable: NodeId) {
        self.contract_sites.push(ContractSite {
            kind: ContractKind::HttpCall,
            url_template: Some(name.to_string()),
            topic: None,
            http_method: Some(method.to_string()),
            messaging_framework: None,
            url_parts: None,
            via_wrapper: None,
            in_callable,
            range: range_of(node),
        });
    }

    /// Emit a `DbTable` node (deduplicated per file). `db_table_id` upper-cases
    /// the name, matching the Java/JPA table ids.
    fn emit_db_table(&mut self, table: &str, file: &str, range: Range) {
        let id = db_table_id(table);
        if self.seen_db_tables.insert(id.as_str().to_string()) {
            self.nodes.push(Node {
                id,
                kind: NodeKind::DbTable,
                name: table.to_string(),
                qualified_name: None,
                file: file.to_string(),
                range,
                props: None,
            });
        }
    }

    /// Emit a `DbQuery` node + `ExecutesQuery` (caller→query) and
    /// `Reads/WritesTable` (query→table) edges, ensuring the `DbTable` exists.
    /// Mirrors `cih_resolve::emit_db_access` so JS DB nodes match Java's.
    fn emit_db_query(
        &mut self,
        node: TsNode<'_>,
        table: &str,
        op: &str,
        engine: &str,
        is_write: bool,
        in_callable: &NodeId,
    ) {
        let range = range_of(node);
        let query_id = db_query_inline_id(&self.rel, range.start_line, range.start_col);
        self.nodes.push(Node {
            id: query_id.clone(),
            kind: NodeKind::DbQuery,
            name: op.to_string(),
            qualified_name: None,
            file: self.rel.clone(),
            range,
            props: Some(serde_json::json!({ "op": op, "engine": engine })),
        });
        self.edges.push(Edge {
            src: in_callable.clone(),
            dst: query_id.clone(),
            kind: EdgeKind::ExecutesQuery,
            confidence: 1.0,
            reason: format!("{engine}-{op}"),
            props: None,
        });
        self.emit_db_table(table, "", Range::default());
        self.edges.push(Edge {
            src: query_id,
            dst: db_table_id(table),
            kind: if is_write {
                EdgeKind::WritesTable
            } else {
                EdgeKind::ReadsTable
            },
            confidence: 1.0,
            reason: format!("{engine}-orm"),
            props: None,
        });
    }

    /// Emit an `EventPublish`/`EventListen` contract site. The resolver turns
    /// these (topic-keyed) into `KafkaTopic` nodes + `PublishesEvent`/`ListensTo`
    /// edges — the same path Java Kafka/Spring events use.
    fn emit_event_contract(
        &mut self,
        node: TsNode<'_>,
        topic: String,
        framework: MessagingFramework,
        is_publish: bool,
        in_callable: NodeId,
    ) {
        self.contract_sites.push(ContractSite {
            kind: if is_publish {
                ContractKind::EventPublish
            } else {
                ContractKind::EventListen
            },
            url_template: None,
            topic: Some(topic),
            http_method: None,
            messaging_framework: Some(framework),
            url_parts: None,
            via_wrapper: None,
            in_callable,
            range: range_of(node),
        });
    }

    /// Framework stereotype for a class: NestJS/Angular/GraphQL decorators
    /// (Angular vs Nest `@Injectable` disambiguated by import) or a React class
    /// component (`extends Component`).
    fn class_stereotype(
        &self,
        node: TsNode<'_>,
        src: &str,
        decorators: &[(String, Option<String>)],
    ) -> Option<String> {
        for (dn, _) in decorators {
            let s = match dn.as_str() {
                "Controller" => "nestjs_controller",
                "Component" => "angular_component",
                "Directive" => "angular_directive",
                "Pipe" => "angular_pipe",
                "NgModule" => "angular_module",
                "Resolver" => "graphql_resolver",
                "Injectable" => {
                    if self.imports_pkg("@angular/core") {
                        "angular_injectable"
                    } else {
                        "nestjs_injectable"
                    }
                }
                _ => continue,
            };
            return Some(s.to_string());
        }
        if self.imports_pkg("react") && class_extends_react_component(node, src) {
            return Some("react_component".to_string());
        }
        None
    }

    /// Emit constructor-injection `TypeRef` reference sites for a provider class:
    /// each `constructor(private x: Dep)` param type becomes a ref from the class,
    /// which the resolver turns into a `Uses` edge — the JS analog of Spring DI.
    fn emit_constructor_di_refs(&mut self, class_node: TsNode<'_>, src: &str, fqn: &str) {
        let class_id = type_id(NodeKind::Class, fqn);
        let Some(body) = class_node.child_by_field_name("body") else {
            return;
        };
        let mut bc = body.walk();
        for member in body.named_children(&mut bc) {
            if member.kind() != "method_definition" {
                continue;
            }
            let is_ctor = member
                .child_by_field_name("name")
                .map(|n| text(n, src))
                .as_deref()
                == Some("constructor");
            if !is_ctor {
                continue;
            }
            let Some(params) = member.child_by_field_name("parameters") else {
                return;
            };
            let mut pc = params.walk();
            for p in params.named_children(&mut pc) {
                if let Some(ty) = param_type_name(p, src) {
                    self.reference_sites.push(ReferenceSite {
                        name: ty,
                        receiver: None,
                        kind: RefKind::TypeRef,
                        arity: None,
                        range: range_of(p),
                        in_fqcn: fqn.to_string(),
                        in_callable: class_id.clone(),
                        arg_texts: Vec::new(),
                    });
                }
            }
            return;
        }
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
        // The module-path `RawImport` (kept for framework detection + the
        // namespace-alias wrapper path) is always emitted. Additionally, for a
        // RELATIVE specifier, each *non-aliased* named import and the default
        // import gets a resolvable module-qualified `RawImport`
        // (`<resolved-module>.<Local>`) so `build_import_map` keys the local
        // symbol to the target type's FQCN — the JS/TS analog of Java's qualified
        // imports (aliased names are skipped; `build_import_map` can't key a local
        // alias to a differently-named export).
        let mut from_path = None;
        let mut alias = None;
        let mut locals: Vec<String> = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "string" => from_path = Some(unquote(&text(child, src))),
                "import_clause" => {
                    let mut clause_cursor = child.walk();
                    for clause_child in child.named_children(&mut clause_cursor) {
                        match clause_child.kind() {
                            // `import Foo from './m'` — default binding local name.
                            "identifier" => locals.push(text(clause_child, src)),
                            "namespace_import" => {
                                let mut ns_cursor = clause_child.walk();
                                alias = clause_child
                                    .named_children(&mut ns_cursor)
                                    .find(|inner| inner.kind() == "identifier")
                                    .map(|inner| text(inner, src));
                            }
                            "named_imports" => {
                                let mut ni = clause_child.walk();
                                for spec in clause_child.named_children(&mut ni) {
                                    if spec.kind() != "import_specifier" {
                                        continue;
                                    }
                                    // Aliased (`X as Y`) can't be keyed cleanly — skip.
                                    if spec.child_by_field_name("alias").is_some() {
                                        continue;
                                    }
                                    if let Some(name) = spec.child_by_field_name("name") {
                                        locals.push(text(name, src));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let raw = from_path.clone().unwrap_or_else(|| text(node, src));
        self.imports.push(RawImport {
            raw,
            is_static: false,
            is_wildcard: false,
            alias,
            range: range_of(node),
        });

        // Resolvable per-symbol imports for relative specifiers only (external
        // package symbols can't map to in-repo FQCNs).
        if let Some(spec) = from_path.filter(|s| s.starts_with('.')) {
            if let Some(module) = crate::constant_resolver::resolve_relative_module(
                std::path::Path::new(&self.rel),
                &spec,
            ) {
                for local in locals {
                    self.imports.push(RawImport {
                        raw: format!("{module}.{local}"),
                        is_static: false,
                        is_wildcard: false,
                        alias: None,
                        range: range_of(node),
                    });
                }
            }
        }
    }

    /// Resolve the enclosing scope for a reference site: the enclosing function's
    /// `(node id, callable signature)` when inside one, else `(file id, module)`.
    /// The signature (`fqcn#name/arity`) is what the resolver keys `type_bindings`
    /// and `this`/receiver resolution on — using the function scope (not the
    /// module) is what makes typed-receiver and `this.method()` calls resolve.
    fn call_scope(&self, enclosing_fn: Option<&NodeId>) -> (NodeId, String) {
        match enclosing_fn {
            Some(fn_id) => {
                let sig = fn_id
                    .as_str()
                    .strip_prefix("Function:")
                    .unwrap_or(&self.module)
                    .to_string();
                (fn_id.clone(), sig)
            }
            None => (file_id(&self.rel), self.module.clone()),
        }
    }

    fn emit_call_reference(&mut self, node: TsNode<'_>, src: &str, enclosing_fn: Option<&NodeId>) {
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
        let (in_callable, in_fqcn) = self.call_scope(enclosing_fn);
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

    /// Emit a `Ctor` reference for `new X(...)` / `new a.B(...)` — resolved to the
    /// type's constructor by the resolver (type-name resolution, not receiver).
    fn emit_ctor_reference(&mut self, node: TsNode<'_>, src: &str, enclosing_fn: Option<&NodeId>) {
        let Some(ctor) = node.child_by_field_name("constructor") else {
            return;
        };
        // Simple type name: `User` → User; `a.B` → B (the resolver keys on it).
        let name = match ctor.kind() {
            "identifier" => text(ctor, src),
            "member_expression" => ctor
                .child_by_field_name("property")
                .map(|p| text(p, src))
                .unwrap_or_default(),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        let (in_callable, in_fqcn) = self.call_scope(enclosing_fn);
        self.reference_sites.push(ReferenceSite {
            name,
            receiver: None,
            kind: RefKind::Ctor,
            arity: call_arity(node),
            range: range_of(ctor),
            in_fqcn,
            in_callable,
            arg_texts: Vec::new(),
        });
    }

    /// Emit `type_bindings` for a callable's typed formal parameters
    /// (`f(u: User)` → `u : User`). `sig` is the callable signature the resolver
    /// keys receiver lookups on. Primitive annotations (`n: number`) are skipped.
    fn emit_param_bindings(&mut self, fn_node: TsNode<'_>, src: &str, sig: &str) {
        let Some(params) = fn_node.child_by_field_name("parameters") else {
            return;
        };
        let mut cursor = params.walk();
        for p in params.named_children(&mut cursor) {
            if !matches!(p.kind(), "required_parameter" | "optional_parameter") {
                continue;
            }
            let (Some(pat), Some(ty)) = (
                p.child_by_field_name("pattern"),
                p.child_by_field_name("type").and_then(|a| type_annotation_name(a, src)),
            ) else {
                continue;
            };
            if pat.kind() != "identifier" {
                continue;
            }
            self.type_bindings.push(TypeBinding {
                name: text(pat, src),
                raw_type: ty,
                kind: BindingKind::Param,
                in_fqcn: sig.to_string(),
                range: range_of(p),
            });
        }
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

/// Fetch-like bare-identifier client whose method comes from the options object.
/// `fetch`/`axios`/`$fetch`/`ofetch` are distinctive enough to match unconditionally;
/// `got`/`ky` are import-gated (checked by the caller) as they collide with common names.
fn fetch_like_identifier(callee: &str) -> bool {
    matches!(callee, "fetch" | "axios" | "$fetch" | "ofetch")
}

/// The receiver name of a member call for HttpClient detection: a bare identifier
/// (`http.get`) or a `this.<name>` member (`this.http.get`).
fn httpclient_receiver_name(object_node: TsNode<'_>, src: &str) -> Option<String> {
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
fn resolve_client_call(
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
            if let Some(v) = axios_http_verb(&prop) {
                return Some((v.to_string(), None));
            }
        }
    }

    // Identifier-receiver clients.
    if object_node.kind() == "identifier" {
        let obj = text(object_node, src);
        if obj == "axios" {
            return axios_http_verb(&prop).map(|v| (v.to_string(), None));
        }
        if let Some(base) = builder.axios_instances.get(&obj) {
            return axios_http_verb(&prop).map(|v| (v.to_string(), base.clone()));
        }
        if (obj == "ky" && builder.imports_pkg("ky"))
            || (obj == "superagent" && builder.imports_pkg("superagent"))
        {
            return axios_http_verb(&prop).map(|v| (v.to_string(), None));
        }
        // undici: `undici.request(url, { method })` (method from options).
        if obj == "undici" && builder.imports_pkg("undici") && prop == "request" {
            return Some((call_options_method(node, src).unwrap_or_else(|| "GET".into()), None));
        }
    }
    None
}

/// Join a client baseURL with a call path (skips absolute URLs on the path side).
fn join_client_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// True if `value` is an `axios.create(...)` call.
fn is_axios_create(value: TsNode<'_>, src: &str) -> bool {
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
fn axios_create_base_url(value: TsNode<'_>, src: &str) -> Option<String> {
    let arg0 = ts_positional_argument(value, 0)?;
    if arg0.kind() != "object" {
        return None;
    }
    literal_ts_string(object_pair_value(arg0, "baseURL", src)?, src)
}

/// Pre-pass: record `const X = axios.create({ baseURL })` instances (name →
/// optional literal baseURL) so their `.get/.post/…` calls resolve as axios.
fn collect_axios_instances(root: TsNode<'_>, src: &str, builder: &mut Builder) {
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

fn try_emit_http_contract(
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
fn trpc_procedure_name(call: TsNode<'_>, src: &str) -> Option<String> {
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
fn try_emit_trpc_consumer(
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
fn try_emit_graphql_consumer(
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
fn graphql_root_op(body: &str) -> Option<(&'static str, String)> {
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
fn ts_arg_is_url_ish(node: TsNode<'_>, src: &str, consts: &std::collections::HashSet<&str>) -> bool {
    let mut parts = Vec::new();
    fold_ts_url_expr(node, src, &mut parts, consts);
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
fn ts_url_parts(
    node: TsNode<'_>,
    src: &str,
    consts: &std::collections::HashSet<&str>,
) -> Option<Vec<UrlPart>> {
    let mut parts = Vec::new();
    fold_ts_url_expr(node, src, &mut parts, consts);
    parts
        .iter()
        .any(|part| !matches!(part, UrlPart::Lit(_)))
        .then_some(parts)
}

fn fold_ts_url_expr(
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
            // becomes a StringConstant for cross-file URL folding.
            if class_fqn.is_none() && enclosing_fn.is_none() {
                collect_module_string_constants(node, src, builder);
            }
            let mut cursor = node.walk();
            for declarator in node.named_children(&mut cursor) {
                if declarator.kind() != "variable_declarator" {
                    walk(declarator, src, builder, class_fqn, controller_prefix, enclosing_fn);
                    continue;
                }
                let name_node = declarator.child_by_field_name("name");
                let value = declarator.child_by_field_name("value");

                // Typed local (`const x: Order = …`) → type_binding scoped to the
                // enclosing callable, so `x.method()` resolves its receiver type.
                if let (Some(nn), Some(ty)) = (
                    name_node.filter(|n| n.kind() == "identifier"),
                    declarator
                        .child_by_field_name("type")
                        .and_then(|a| type_annotation_name(a, src)),
                ) {
                    let (_, sig) = builder.call_scope(enclosing_fn);
                    builder.type_bindings.push(TypeBinding {
                        name: text(nn, src),
                        raw_type: ty,
                        kind: BindingKind::Local,
                        in_fqcn: sig,
                        range: range_of(declarator),
                    });
                }

                // `export const apiFetch = async (endpoint, …) => …` wrapper shape.
                if class_fqn.is_none() && enclosing_fn.is_none() {
                    if let (Some(nn), Some(v)) = (name_node, value) {
                        if nn.kind() == "identifier" && v.kind() == "arrow_function" {
                            try_collect_http_wrapper(&text(nn, src), v, src, builder);
                        }
                    }
                }

                // React component/hook defined as an arrow/function const
                // (`const Card = () => …`, `const useAuth = () => …`). These are
                // not otherwise emitted as nodes, so the P4 stereotype missed
                // them; emit the Function node and walk its body attributed to it.
                let component = name_node.zip(value).and_then(|(nn, v)| {
                    if class_fqn.is_none()
                        && nn.kind() == "identifier"
                        && matches!(
                            v.kind(),
                            "arrow_function" | "function" | "function_expression"
                        )
                    {
                        let name = text(nn, src);
                        react_function_stereotype(&name, builder).map(|s| (name, v, s))
                    } else {
                        None
                    }
                });

                if let Some((name, v, stereo)) = component {
                    let arity = parameter_count(v);
                    let fn_id = builder.emit_function(v, src, &name, arity, None, Some(&stereo));
                    if let Some(body) = v.child_by_field_name("body") {
                        walk(body, src, builder, class_fqn, controller_prefix, Some(&fn_id));
                    }
                } else {
                    walk(declarator, src, builder, class_fqn, controller_prefix, enclosing_fn);
                }
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

            let stereotype = builder.class_stereotype(node, src, &decorators);
            let fqn = builder.emit_class(node, src, &class_name, stereotype.as_deref());
            let class_id = type_id(NodeKind::Class, &fqn);
            builder.emit_heritage(node, src, &fqn, &class_id);
            builder.emit_class_fields(node, src, &fqn, &class_id);

            // TypeORM / sequelize-typescript entity: `@Entity('t')` / `@Table('t')`
            // → DbTable (arg overrides the class name).
            if let Some((_, arg)) = decorators
                .iter()
                .find(|(n, _)| n == "Entity" || n == "Table")
            {
                let table = arg.clone().unwrap_or_else(|| class_name.clone());
                builder.emit_db_table(&table, &builder.rel.clone(), range_of(node));
            }

            // Constructor DI: provider classes wire in their injected dependencies.
            if is_di_provider(stereotype.as_deref()) {
                builder.emit_constructor_di_refs(node, src, &fqn);
            }

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
            // React component/hook stereotype (top-level functions only).
            let stereotype = if class_fqn.is_none() {
                react_function_stereotype(&name, builder)
            } else {
                None
            };
            let fn_id =
                builder.emit_function(node, src, &name, arity, class_fqn, stereotype.as_deref());

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
                    builder.emit_operation_route(
                        node,
                        RouteSource::GraphQl,
                        op,
                        &opname,
                        Some(&fn_id),
                    );
                }
                // NestJS microservice / WebSocket message handlers → EventListen.
                if matches!(
                    dname.as_str(),
                    "MessagePattern" | "EventPattern" | "SubscribeMessage"
                ) {
                    let topic = dpath.clone().unwrap_or_else(|| name.clone());
                    builder.emit_event_contract(
                        node,
                        topic,
                        MessagingFramework::NestMicroservice,
                        false,
                        fn_id.clone(),
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
            let fn_id = builder.emit_function(node, src, &name, arity, class_fqn, None);

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
                    builder.emit_operation_route(
                        node,
                        RouteSource::GraphQl,
                        op,
                        &opname,
                        Some(&fn_id),
                    );
                }
                // NestJS microservice / WebSocket message handlers → EventListen.
                if matches!(
                    dname.as_str(),
                    "MessagePattern" | "EventPattern" | "SubscribeMessage"
                ) {
                    let topic = dpath.clone().unwrap_or_else(|| name.clone());
                    builder.emit_event_contract(
                        node,
                        topic,
                        MessagingFramework::NestMicroservice,
                        false,
                        fn_id.clone(),
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
            try_emit_trpc_consumer(node, src, builder, enclosing_fn);
            try_emit_graphql_consumer(node, src, builder, enclosing_fn);
            try_emit_db_query(node, src, builder, enclosing_fn);
            try_emit_messaging(node, src, builder, enclosing_fn);
            builder.emit_call_reference(node, src, enclosing_fn);
            // recurse into arguments
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
            }
        }
        "new_expression" => {
            builder.emit_ctor_reference(node, src, enclosing_fn);
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

    // Pre-pass: axios.create() instances and ORM model vars must be known before
    // their calls are visited during the walk.
    collect_axios_instances(tree.root_node(), src, &mut builder);
    collect_db_models(tree.root_node(), src, &mut builder);
    collect_queue_instances(tree.root_node(), src, &mut builder);

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
            type_bindings: builder.type_bindings,
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
    fn graphql_resolver_routes() {
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
        assert!(
            has_route(&unit, "Route:graphql:QUERY:users"),
            "graphql query route missing: {:?}",
            route_ids(&unit)
        );
        assert!(
            has_route(&unit, "Route:graphql:MUTATION:createUser"),
            "graphql mutation route missing: {:?}",
            route_ids(&unit)
        );
        // HandlesRoute edge from the resolver method to the operation.
        assert!(
            unit.edges.iter().any(|e| e.kind == cih_core::EdgeKind::HandlesRoute
                && e.dst.as_str().contains("graphql")),
            "graphql HandlesRoute edge missing"
        );
    }

    // ── P2: outbound HTTP clients ────────────────────────────────────────────

    fn http_calls(unit: &cih_core::ParsedUnit) -> Vec<(String, String)> {
        unit.parsed_file
            .contract_sites
            .iter()
            .filter(|c| matches!(c.kind, cih_core::ContractKind::HttpCall))
            .map(|c| {
                (
                    c.http_method.clone().unwrap_or_default(),
                    c.url_template.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn axios_create_instance_folds_base_url() {
        let src = r#"import axios from 'axios';
const api = axios.create({ baseURL: '/api/v1' });
export async function load() { return api.get('/orders/1'); }
"#;
        let unit = parse_typescript_file("src/api.ts", src).expect("parses");
        let calls = http_calls(&unit);
        assert!(
            calls
                .iter()
                .any(|(m, u)| m == "GET" && u == "/api/v1/orders/1"),
            "axios instance call with folded baseURL missing: {calls:?}"
        );
    }

    #[test]
    fn angular_httpclient_this_http() {
        let src = r#"import { HttpClient } from '@angular/common/http';
class UserService {
    constructor(private http: HttpClient) {}
    load() { return this.http.get('/api/users'); }
    create() { return this.http.post('/api/users', {}); }
}
"#;
        let unit = parse_typescript_file("src/user.service.ts", src).expect("parses");
        let calls = http_calls(&unit);
        assert!(
            calls.iter().any(|(m, u)| m == "GET" && u == "/api/users")
                && calls.iter().any(|(m, u)| m == "POST" && u == "/api/users"),
            "angular HttpClient calls missing: {calls:?}"
        );
    }

    #[test]
    fn typed_fields_and_ctor_param_properties() {
        let src = r#"class Svc {
  private field: Repo;
  http: HttpClient;
  x = 1;
  constructor(private param: Mailer, plain: number) {}
}
"#;
        let unit = parse_typescript_file("src/svc.ts", src).expect("parses");
        let fields: Vec<(String, Option<String>)> = unit
            .parsed_file
            .defs
            .iter()
            .filter(|d| d.kind == cih_core::NodeKind::Field)
            .map(|d| (d.name.clone(), d.declared_type.clone()))
            .collect();
        let has = |n: &str, t: &str| {
            fields
                .iter()
                .any(|(fn_, ft)| fn_ == n && ft.as_deref() == Some(t))
        };
        assert!(has("field", "Repo"), "typed field: {fields:?}");
        assert!(has("http", "HttpClient"), "typed field: {fields:?}");
        assert!(has("param", "Mailer"), "ctor param property: {fields:?}");
        // Untyped field `x = 1` → no field def (no resolvable type).
        assert!(!fields.iter().any(|(n, _)| n == "x"), "{fields:?}");
        // Plain ctor param `plain: number` (no accessibility modifier) → not a field.
        assert!(!fields.iter().any(|(n, _)| n == "plain"), "{fields:?}");
    }

    #[test]
    fn class_and_interface_heritage_refs() {
        let src = r#"export class Admin extends User implements Named, Other {}
interface I extends Base {}
class W extends React.Component<P> {}
"#;
        let unit = parse_typescript_file("src/app.ts", src).expect("parses");
        let refs: Vec<(cih_core::RefKind, String, String)> = unit
            .parsed_file
            .reference_sites
            .iter()
            .filter(|r| matches!(r.kind, cih_core::RefKind::Extends | cih_core::RefKind::Implements))
            .map(|r| (r.kind, r.name.clone(), r.in_fqcn.clone()))
            .collect();
        let has = |k: cih_core::RefKind, n: &str, f: &str| {
            refs.iter().any(|(rk, rn, rf)| *rk == k && rn == n && rf == f)
        };
        assert!(has(cih_core::RefKind::Extends, "User", "src/app.Admin"), "{refs:?}");
        assert!(has(cih_core::RefKind::Implements, "Named", "src/app.Admin"), "{refs:?}");
        assert!(has(cih_core::RefKind::Implements, "Other", "src/app.Admin"), "{refs:?}");
        assert!(has(cih_core::RefKind::Extends, "Base", "src/app.I"), "{refs:?}");
        // Member-expression + generic base → simple name.
        assert!(has(cih_core::RefKind::Extends, "Component", "src/app.W"), "{refs:?}");
    }

    #[test]
    fn method_params_new_and_scope_aware_calls() {
        let src = r#"class Svc {
  handle(u: User) { u.save(); }
  make() { const x = new User(1); x.load(); return x; }
}
"#;
        let unit = parse_typescript_file("src/svc.ts", src).expect("parses");
        let pf = &unit.parsed_file;
        // Typed param → Param type_binding scoped to the method signature.
        assert!(
            pf.type_bindings.iter().any(|b| b.name == "u"
                && b.raw_type == "User"
                && b.kind == cih_core::BindingKind::Param
                && b.in_fqcn == "src/svc.Svc#handle/1"),
            "param binding missing: {:?}",
            pf.type_bindings
        );
        // Typed local `const x: … = new User()` has no annotation here, but the
        // `new User(1)` emits a Ctor reference.
        assert!(
            pf.reference_sites
                .iter()
                .any(|r| r.kind == cih_core::RefKind::Ctor && r.name == "User"),
            "ctor ref for `new User()` missing"
        );
        // Call refs are scoped to the enclosing method (not the module), which is
        // what makes `this.x()` / typed-receiver resolution work.
        assert!(
            pf.reference_sites.iter().any(|r| r.kind == cih_core::RefKind::Call
                && r.name == "save"
                && r.in_fqcn == "src/svc.Svc#handle/1"),
            "call ref not scoped to method: {:?}",
            pf.reference_sites
                .iter()
                .filter(|r| r.name == "save")
                .map(|r| &r.in_fqcn)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn typed_local_emits_binding() {
        let src = r#"function f() { const o: Order = load(); o.total(); }"#;
        let unit = parse_typescript_file("src/f.ts", src).expect("parses");
        assert!(
            unit.parsed_file.type_bindings.iter().any(|b| b.name == "o"
                && b.raw_type == "Order"
                && b.kind == cih_core::BindingKind::Local
                && b.in_fqcn == "src/f#f/0"),
            "typed local binding missing: {:?}",
            unit.parsed_file.type_bindings
        );
    }

    #[test]
    fn relative_named_imports_emit_resolvable_qualified_raws() {
        let src = r#"import { OrderService, Foo as F } from './services/order';
import def from './x';
import ext from 'express';
import * as api from './client';
"#;
        let unit = parse_typescript_file("src/app/caller.ts", src).expect("parses");
        let raws: Vec<&str> = unit.parsed_file.imports.iter().map(|i| i.raw.as_str()).collect();
        // Relative non-aliased named import → module-qualified FQCN (build_import_map
        // then keys `OrderService` → this).
        assert!(
            raws.contains(&"src/app/services/order.OrderService"),
            "named import not qualified: {raws:?}"
        );
        // Default import → qualified by local name.
        assert!(raws.contains(&"src/app/x.def"), "default import not qualified: {raws:?}");
        // Aliased (`Foo as F`) is skipped (can't key a local alias cleanly).
        assert!(
            !raws.iter().any(|r| r.ends_with(".F") || r.ends_with(".Foo")),
            "aliased import should be skipped: {raws:?}"
        );
        // External package: module path kept, no synthetic symbol FQCN.
        assert!(raws.contains(&"express"));
        assert!(
            !raws.iter().any(|r| r.starts_with("express.")),
            "external symbols must not be qualified: {raws:?}"
        );
        // Namespace import stays a plain module path (alias handled separately).
        assert!(raws.contains(&"./client"));
    }

    #[test]
    fn in_file_const_template_folds_param_stays_dynamic() {
        // `${apiBase}` (a same-file module const) → ConstRef (folds at resolve);
        // `${userId}` (a param) → Dynamic → stays `{*}`.
        let src = r#"const apiBase = '/api/v2';
export async function a() { return fetch(`${apiBase}/users`); }
export async function b(userId) { return fetch(`/api/users/${userId}`); }
"#;
        let unit = parse_typescript_file("src/api.ts", src).expect("parses");
        let all_parts: Vec<&cih_core::UrlPart> = unit
            .parsed_file
            .contract_sites
            .iter()
            .filter_map(|c| c.url_parts.as_ref())
            .flatten()
            .collect();
        assert!(
            all_parts
                .iter()
                .any(|p| matches!(p, cih_core::UrlPart::ConstRef(n) if n == "apiBase")),
            "in-file const apiBase should be a ConstRef: {all_parts:?}"
        );
        assert!(
            !all_parts
                .iter()
                .any(|p| matches!(p, cih_core::UrlPart::ConstRef(n) if n == "userId")),
            "param userId must stay Dynamic, not a ConstRef: {all_parts:?}"
        );
    }

    #[test]
    fn got_import_gated_client() {
        let src = r#"import got from 'got';
export async function f() { return got('http://svc/data', { method: 'POST' }); }
"#;
        let unit = parse_typescript_file("src/g.ts", src).expect("parses");
        assert!(
            http_calls(&unit).iter().any(|(m, _)| m == "POST"),
            "got POST call missing: {:?}",
            http_calls(&unit)
        );
    }

    #[test]
    fn plain_http_get_not_a_client_without_import() {
        // `http.get` with no @angular/@nestjs import must NOT emit (Node's http core).
        let src = r#"import http from 'http';
export function f() { return http.get('http://x/y'); }
"#;
        let unit = parse_typescript_file("src/n.ts", src).expect("parses");
        assert!(
            http_calls(&unit).is_empty(),
            "node http.get must not be treated as a client: {:?}",
            http_calls(&unit)
        );
    }

    // ── P3: DB / ORM access ──────────────────────────────────────────────────

    fn db_table_ids(unit: &cih_core::ParsedUnit) -> Vec<String> {
        unit.nodes
            .iter()
            .filter(|n| n.kind == cih_core::NodeKind::DbTable)
            .map(|n| n.id.as_str().to_string())
            .collect()
    }

    fn has_db_query_edge(unit: &cih_core::ParsedUnit, kind: cih_core::EdgeKind) -> bool {
        unit.edges.iter().any(|e| e.kind == kind)
    }

    #[test]
    fn prisma_query_emits_table_and_edges() {
        let src = r#"import { PrismaClient } from '@prisma/client';
const prisma = new PrismaClient();
export async function list() { return prisma.user.findMany(); }
export async function make(d) { return prisma.order.create({ data: d }); }
"#;
        let unit = parse_typescript_file("src/repo.ts", src).expect("parses");
        let tables = db_table_ids(&unit);
        assert!(tables.contains(&"DbTable:USER".to_string()), "USER table: {tables:?}");
        assert!(tables.contains(&"DbTable:ORDER".to_string()), "ORDER table: {tables:?}");
        assert!(has_db_query_edge(&unit, cih_core::EdgeKind::ReadsTable));
        assert!(has_db_query_edge(&unit, cih_core::EdgeKind::WritesTable));
        assert!(has_db_query_edge(&unit, cih_core::EdgeKind::ExecutesQuery));
    }

    #[test]
    fn mongoose_model_var_query() {
        let src = r#"import mongoose from 'mongoose';
const User = mongoose.model('User', new mongoose.Schema({}));
export async function find(id) { return User.findById(id); }
"#;
        let unit = parse_typescript_file("src/user.model.ts", src).expect("parses");
        assert!(
            db_table_ids(&unit).contains(&"DbTable:USER".to_string()),
            "mongoose table missing: {:?}",
            db_table_ids(&unit)
        );
        assert!(has_db_query_edge(&unit, cih_core::EdgeKind::ReadsTable));
    }

    #[test]
    fn sequelize_define_write() {
        let src = r#"const Order = sequelize.define('orders', {});
export async function add(d) { return Order.create(d); }
"#;
        let unit = parse_typescript_file("src/order.ts", src).expect("parses");
        assert!(db_table_ids(&unit).contains(&"DbTable:ORDERS".to_string()));
        assert!(has_db_query_edge(&unit, cih_core::EdgeKind::WritesTable));
    }

    #[test]
    fn knex_chained_query_finds_table() {
        let src = r#"import knex from 'knex';
export async function get(id) { return knex('products').where('id', id).select(); }
"#;
        let unit = parse_typescript_file("src/products.ts", src).expect("parses");
        assert!(
            db_table_ids(&unit).contains(&"DbTable:PRODUCTS".to_string()),
            "knex table missing: {:?}",
            db_table_ids(&unit)
        );
    }

    #[test]
    fn typeorm_entity_table() {
        let src = r#"import { Entity, Column } from 'typeorm';
@Entity('users')
class User { }
"#;
        let unit = parse_typescript_file("src/user.entity.ts", src).expect("parses");
        assert!(
            db_table_ids(&unit).contains(&"DbTable:USERS".to_string()),
            "typeorm entity table missing: {:?}",
            db_table_ids(&unit)
        );
    }

    #[test]
    fn plain_array_find_is_not_a_db_query() {
        // `.find` on a plain array must NOT emit a DbQuery (no model/prisma/knex).
        let src = r#"export function f(xs) { return xs.find(x => x.id === 1); }"#;
        let unit = parse_typescript_file("src/util.ts", src).expect("parses");
        assert!(
            db_table_ids(&unit).is_empty()
                && !has_db_query_edge(&unit, cih_core::EdgeKind::ReadsTable),
            "array .find must not be a DB query"
        );
    }

    // ── P4: component stereotypes + DI ───────────────────────────────────────

    fn stereotype_of(unit: &cih_core::ParsedUnit, name: &str) -> Option<String> {
        unit.nodes
            .iter()
            .find(|n| n.name == name)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    #[test]
    fn angular_component_stereotype() {
        let src = r#"import { Component } from '@angular/core';
@Component({ selector: 'app-root' })
class AppComponent {}
"#;
        let unit = parse_typescript_file("src/app.component.ts", src).expect("parses");
        assert_eq!(
            stereotype_of(&unit, "AppComponent").as_deref(),
            Some("angular_component")
        );
    }

    #[test]
    fn nest_injectable_di_refs() {
        // Exported form (`@Dec() export class`) — the common real-world shape.
        let src = r#"import { Injectable } from '@nestjs/common';
@Injectable()
export class UserService {
    constructor(private readonly repo: UserRepository, private mailer: Mailer) {}
}
"#;
        let unit = parse_typescript_file("src/user.service.ts", src).expect("parses");
        assert_eq!(
            stereotype_of(&unit, "UserService").as_deref(),
            Some("nestjs_injectable")
        );
        let type_refs: Vec<&str> = unit
            .parsed_file
            .reference_sites
            .iter()
            .filter(|r| r.kind == cih_core::RefKind::TypeRef)
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            type_refs.contains(&"UserRepository") && type_refs.contains(&"Mailer"),
            "DI constructor type refs missing: {type_refs:?}"
        );
    }

    #[test]
    fn react_function_component_and_hook() {
        let src = r#"import React from 'react';
export function Card() { return null; }
export function useAuth() { return true; }
export function helper() { return 1; }
"#;
        let unit = parse_typescript_file("src/ui.tsx", src).expect("parses");
        assert_eq!(stereotype_of(&unit, "Card").as_deref(), Some("react_component"));
        assert_eq!(stereotype_of(&unit, "useAuth").as_deref(), Some("react_hook"));
        assert_eq!(stereotype_of(&unit, "helper"), None); // lowercase, not a component
    }

    #[test]
    fn react_arrow_const_component_and_hook() {
        // The dominant React shape: components/hooks as `const X = () => …`.
        let src = r#"import React from 'react';
export const Card = ({ title }) => null;
export const useAuth = () => true;
const helper = () => 1;
"#;
        let unit = parse_typescript_file("src/ui.tsx", src).expect("parses");
        assert_eq!(
            stereotype_of(&unit, "Card").as_deref(),
            Some("react_component"),
            "arrow-const component not labeled"
        );
        assert_eq!(
            stereotype_of(&unit, "useAuth").as_deref(),
            Some("react_hook"),
            "arrow-const hook not labeled"
        );
        // lowercase non-component arrow const is NOT emitted as a node.
        assert!(
            !unit.nodes.iter().any(|n| n.name == "helper"),
            "lowercase arrow const should not be emitted"
        );
    }

    #[test]
    fn arrow_const_contract_attributes_to_component() {
        // A fetch inside an arrow component now attributes to the component fn,
        // not the file (arrow functions were untracked before).
        let src = r#"import React from 'react';
export const UserList = () => {
    fetch('/api/users');
    return null;
};
"#;
        let unit = parse_typescript_file("src/list.tsx", src).expect("parses");
        let site = unit
            .parsed_file
            .contract_sites
            .iter()
            .find(|c| matches!(c.kind, cih_core::ContractKind::HttpCall))
            .expect("fetch contract site");
        assert!(
            site.in_callable.as_str().contains("UserList"),
            "contract should attribute to UserList, got {}",
            site.in_callable.as_str()
        );
    }

    #[test]
    fn react_class_component_stereotype() {
        let src = r#"import React from 'react';
class Widget extends React.Component { render() { return null; } }
"#;
        let unit = parse_typescript_file("src/widget.tsx", src).expect("parses");
        assert_eq!(
            stereotype_of(&unit, "Widget").as_deref(),
            Some("react_component")
        );
    }

    // ── P5: messaging / realtime ─────────────────────────────────────────────

    fn event_contracts(
        unit: &cih_core::ParsedUnit,
    ) -> Vec<(cih_core::ContractKind, String)> {
        unit.parsed_file
            .contract_sites
            .iter()
            .filter(|c| {
                matches!(
                    c.kind,
                    cih_core::ContractKind::EventPublish | cih_core::ContractKind::EventListen
                )
            })
            .map(|c| (c.kind.clone(), c.topic.clone().unwrap_or_default()))
            .collect()
    }

    #[test]
    fn socketio_emit_and_on() {
        let src = r#"import { Server } from 'socket.io';
export function wire(io) {
    io.on('connection', (socket) => {
        socket.emit('welcome', {});
        socket.on('message', (m) => {});
    });
}
"#;
        let unit = parse_typescript_file("src/gateway.ts", src).expect("parses");
        let evs = event_contracts(&unit);
        assert!(
            evs.iter()
                .any(|(k, t)| *k == cih_core::ContractKind::EventPublish && t == "welcome"),
            "socket emit missing: {evs:?}"
        );
        assert!(
            evs.iter()
                .any(|(k, t)| *k == cih_core::ContractKind::EventListen && t == "message"),
            "socket on missing: {evs:?}"
        );
    }

    #[test]
    fn kafkajs_producer_consumer() {
        let src = r#"import { Kafka } from 'kafkajs';
export async function pub(producer) { await producer.send({ topic: 'orders', messages: [] }); }
export async function sub(consumer) { await consumer.subscribe({ topic: 'orders' }); }
"#;
        let unit = parse_typescript_file("src/kafka.ts", src).expect("parses");
        let evs = event_contracts(&unit);
        assert!(evs
            .iter()
            .any(|(k, t)| *k == cih_core::ContractKind::EventPublish && t == "orders"));
        assert!(evs
            .iter()
            .any(|(k, t)| *k == cih_core::ContractKind::EventListen && t == "orders"));
    }

    #[test]
    fn bull_queue_add_publishes() {
        let src = r#"import { Queue } from 'bullmq';
const emailQueue = new Queue('emails');
export async function enqueue() { await emailQueue.add('send', {}); }
"#;
        let unit = parse_typescript_file("src/queue.ts", src).expect("parses");
        assert!(
            event_contracts(&unit)
                .iter()
                .any(|(k, t)| *k == cih_core::ContractKind::EventPublish && t == "emails"),
            "bull queue.add missing: {:?}",
            event_contracts(&unit)
        );
    }

    #[test]
    fn nest_message_pattern_listen() {
        let src = r#"import { MessagePattern } from '@nestjs/microservices';
class Handler {
    @MessagePattern('order.created')
    handle() {}
}
"#;
        let unit = parse_typescript_file("src/handler.ts", src).expect("parses");
        assert!(
            event_contracts(&unit)
                .iter()
                .any(|(k, t)| *k == cih_core::ContractKind::EventListen && t == "order.created"),
            "nest @MessagePattern missing: {:?}",
            event_contracts(&unit)
        );
    }

    #[test]
    fn socket_emit_not_detected_without_import() {
        // `.emit` with no socket.io import must not be a messaging contract.
        let src = r#"export function f(ee) { ee.emit('data', {}); }"#;
        let unit = parse_typescript_file("src/x.ts", src).expect("parses");
        assert!(event_contracts(&unit).is_empty());
    }

    #[test]
    fn trpc_procedure_routes() {
        let src = r#"import { initTRPC } from '@trpc/server';
const t = initTRPC.create();
export const appRouter = t.router({
    getUser: t.procedure.query(() => ({ id: 1 })),
    setUser: t.procedure.mutation(() => ({ ok: true })),
});
"#;
        let unit = parse_typescript_file("src/router.ts", src).expect("parses");
        assert!(
            has_route(&unit, "Route:trpc:QUERY:getUser")
                && has_route(&unit, "Route:trpc:MUTATION:setUser"),
            "trpc routes missing: {:?}",
            route_ids(&unit)
        );
    }

    #[test]
    fn trpc_consumer_calls() {
        let src = r#"import { createTRPCReact } from '@trpc/react-query';
export const trpc = createTRPCReact();
export function C() {
    const q = trpc.user.getUser.useQuery({ id: 1 });
    const m = trpc.post.create.useMutation();
    return q;
}
"#;
        let unit = parse_typescript_file("src/client.ts", src).expect("parses");
        let calls = http_calls(&unit);
        assert!(
            calls.iter().any(|(m, u)| m == "QUERY" && u == "getUser"),
            "trpc query consumer missing: {calls:?}"
        );
        assert!(
            calls.iter().any(|(m, u)| m == "MUTATION" && u == "create"),
            "trpc mutation consumer missing: {calls:?}"
        );
    }

    #[test]
    fn react_query_usequery_is_not_a_trpc_consumer() {
        let src = r#"import { useQuery } from '@tanstack/react-query';
export function C() { return useQuery({ queryKey: ['x'], queryFn: () => 1 }); }
"#;
        let unit = parse_typescript_file("src/rq.ts", src).expect("parses");
        assert!(
            !http_calls(&unit).iter().any(|(m, _)| m == "QUERY"),
            "bare react-query useQuery must not be a trpc consumer"
        );
    }

    #[test]
    fn graphql_consumer_gql_templates() {
        let src = r#"import { gql } from '@apollo/client';
export const GET_ME = gql`query GetMe { me { id name } }`;
export const CREATE = gql`mutation { createPost(title: "x") { id } }`;
"#;
        let unit = parse_typescript_file("src/queries.ts", src).expect("parses");
        let calls = http_calls(&unit);
        assert!(
            calls.iter().any(|(m, u)| m == "QUERY" && u == "me"),
            "graphql query consumer missing: {calls:?}"
        );
        assert!(
            calls.iter().any(|(m, u)| m == "MUTATION" && u == "createPost"),
            "graphql mutation consumer missing: {calls:?}"
        );
    }
}







