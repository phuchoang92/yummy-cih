use cih_core::{
    type_id, BindingKind, MessagingFramework, NodeId, NodeKind, ParsedFile,
    ParsedUnit, RouteSource, TypeBinding,
};
use tree_sitter::Node as TsNode;

use super::builder::Builder;
use super::helpers::*;
use super::components::*;
use super::db::*;
use super::file_routes::*;
use super::http_clients::*;
use super::messaging::*;
use super::routes::*;






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
            walk_lexical_declaration(node, src, builder, class_fqn, controller_prefix, enclosing_fn)
        }
        "class_declaration" | "abstract_class_declaration" => {
            walk_class_declaration(node, src, builder)
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
            walk_function_declaration(node, src, builder, class_fqn, controller_prefix)
        }
        "method_definition" => {
            walk_method_definition(node, src, builder, class_fqn, controller_prefix)
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
        "assignment_expression" => walk_assignment_expression(
            node,
            src,
            builder,
            class_fqn,
            controller_prefix,
            enclosing_fn,
        ),
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
            }
        }
    }
}

/// `class_declaration` / `abstract_class_declaration` arm of [`walk`]: emit the
/// class node (+ heritage, fields, `@Entity`/`@Table` DbTable, constructor DI),
/// then walk the body under the class's fqn and `@Controller` prefix.
fn walk_class_declaration(node: TsNode<'_>, src: &str, builder: &mut Builder) {
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

/// `assignment_expression` arm of [`walk`]: CommonJS export-assignment defs
/// (`exports.foo = () => …` / `module.exports.foo = function …`) become a
/// module-level Function; barrel re-exports (`module.exports.svc = require(...)`)
/// are recorded; otherwise recurse into children.
fn walk_assignment_expression(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    class_fqn: Option<&str>,
    controller_prefix: Option<&str>,
    enclosing_fn: Option<&NodeId>,
) {
    // CommonJS export-assignment defs: `exports.foo = async () => …` /
    // `module.exports.foo = function () {…}` → a module-level Function node
    // named `foo`, so `require('./m').foo` / `x.foo()` have a callee to
    // resolve against. Attribute the body's calls to the new function.
    let emitted = if class_fqn.is_none() {
        try_emit_exports_function(node, src, builder)
    } else {
        None
    };
    if let Some(fn_id) = emitted {
        // Body comes off the *unwrapped* function — `right` may be the
        // wrapper call (`catchAsync(fn)`), which has no `body` field, and
        // missing that would drop every call inside the handler.
        if let Some(body) = node
            .child_by_field_name("right")
            .and_then(callable_value)
            .and_then(|f| f.child_by_field_name("body"))
        {
            walk(body, src, builder, class_fqn, controller_prefix, Some(&fn_id));
        }
    } else {
        // Not a function export — it may be a barrel re-export
        // (`module.exports.svc = require('./svc')`).
        if class_fqn.is_none() {
            try_emit_exports_reexport(node, src, builder);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk(child, src, builder, class_fqn, controller_prefix, enclosing_fn);
        }
    }
}

/// `lexical_declaration` arm of [`walk`]: module string constants, typed-local
/// type bindings, CommonJS `require` bindings, HTTP-wrapper shapes, and — the
/// dominant modern idiom — callables bound to a `const` (arrow/function-expr).
fn walk_lexical_declaration(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    class_fqn: Option<&str>,
    controller_prefix: Option<&str>,
    enclosing_fn: Option<&NodeId>,
) {
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

        // CommonJS `const x = require('./m')` / `const { a } = require('./m')`
        // / `const f = require('./m').foo` → binding + free-call hints.
        builder.emit_require_binding(declarator, enclosing_fn, src);

        // `export const apiFetch = async (endpoint, …) => …` wrapper shape.
        if class_fqn.is_none() && enclosing_fn.is_none() {
            if let (Some(nn), Some(v)) = (name_node, value) {
                if nn.kind() == "identifier" && v.kind() == "arrow_function" {
                    try_collect_http_wrapper(&text(nn, src), v, src, builder);
                }
            }
        }

        // A callable bound to a const IS a function — emit it, whatever it's
        // named. `const getUser = async () => …`, `const Card = () => …`,
        // and `const h = catchAsync(async (req, res) => …)` are the dominant
        // way modern JS/TS declares functions; before this, only React-named
        // consts became nodes, so most repos had almost no callee nodes at
        // all (a 38-file Express app: 49 arrow consts, 0 `function` decls).
        // React names still get their stereotype — it decorates, it no longer
        // gates. Single emission site: `emit_function` has no dedup guard.
        let callable = name_node
            .zip(value)
            .filter(|(nn, _)| class_fqn.is_none() && nn.kind() == "identifier")
            .and_then(|(nn, v)| {
                let inner = callable_value(v)?;
                Some((text(nn, src), inner))
            });

        if let Some((name, v)) = callable {
            let stereo = react_function_stereotype(&name, builder);
            let arity = parameter_count(v);
            let fn_id = builder.emit_function(v, src, &name, arity, None, stereo.as_deref());
            if let Some(body) = v.child_by_field_name("body") {
                walk(body, src, builder, class_fqn, controller_prefix, Some(&fn_id));
            }
        } else {
            walk(declarator, src, builder, class_fqn, controller_prefix, enclosing_fn);
        }
    }
}

/// `function_declaration` arm of [`walk`]: emit the function node (+ HTTP-wrapper
/// / React stereotype for top-level), its NestJS/GraphQL decorator routes, then
/// walk the body attributing calls to it.
fn walk_function_declaration(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    class_fqn: Option<&str>,
    controller_prefix: Option<&str>,
) {
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
    let fn_id = builder.emit_function(node, src, &name, arity, class_fqn, stereotype.as_deref());

    let ctrl_prefix = controller_prefix.unwrap_or("");
    emit_callable_decorators(node, &decorators, &fn_id, &name, ctrl_prefix, builder);

    // Walk body for call references
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            walk(child, src, builder, class_fqn, controller_prefix, Some(&fn_id));
        }
    }
}

/// `method_definition` arm of [`walk`]: emit the method node, its NestJS/GraphQL
/// decorator routes, then walk the body attributing calls to it.
fn walk_method_definition(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    class_fqn: Option<&str>,
    controller_prefix: Option<&str>,
) {
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

    let ctrl_prefix = controller_prefix.unwrap_or("");
    emit_callable_decorators(node, &decorators, &fn_id, &name, ctrl_prefix, builder);

    // Walk body
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            walk(child, src, builder, class_fqn, controller_prefix, Some(&fn_id));
        }
    }
}

/// Emit routes/contracts declared by a callable's NestJS/GraphQL decorators:
/// `@Get/@Post/…` HTTP routes, `@Query/@Mutation/…` GraphQL operations, and
/// `@MessagePattern/@EventPattern/@SubscribeMessage` event listeners. Shared
/// verbatim by [`walk_function_declaration`] and [`walk_method_definition`].
fn emit_callable_decorators(
    node: TsNode<'_>,
    decorators: &[(String, Option<String>)],
    fn_id: &NodeId,
    name: &str,
    ctrl_prefix: &str,
    builder: &mut Builder,
) {
    for (dname, dpath) in decorators {
        if let Some(http_method) = nestjs_http_method(dname) {
            let method_path = dpath.as_deref().unwrap_or("");
            let full_path = join_paths(ctrl_prefix, method_path);
            builder.emit_nestjs_route(node, fn_id, http_method, &full_path, dname);
        }
        if let Some(op) = graphql_operation(dname) {
            let opname = dpath.clone().unwrap_or_else(|| name.to_string());
            builder.emit_operation_route(node, RouteSource::GraphQl, op, &opname, Some(fn_id));
        }
        // NestJS microservice / WebSocket message handlers → EventListen.
        if matches!(
            dname.as_str(),
            "MessagePattern" | "EventPattern" | "SubscribeMessage"
        ) {
            let topic = dpath.clone().unwrap_or_else(|| name.to_string());
            builder.emit_event_contract(
                node,
                topic,
                MessagingFramework::NestMicroservice,
                false,
                fn_id.clone(),
            );
        }
    }
}

/// The function a declarator value ultimately denotes, unwrapping one layer of
/// higher-order wrapper.
///
/// - `async () => {}` / `function () {}` → itself.
/// - `catchAsync(async (req, res) => {})` → the inner arrow. Express/Koa handlers,
///   `React.memo(fn)` and `forwardRef(fn)` all take this shape, and without it an
///   entire controller layer has no function nodes.
///
/// The wrapper rule requires **exactly one** argument, and that it be a function.
/// That single condition is what keeps it honest: it excludes `useMemo(() => v,
/// [deps])` and `useCallback(fn, [deps])` (two args — they yield a *value*, not a
/// named function), and `require('./m')` (its argument is a string). Anything else
/// returns `None` and is walked normally.
fn callable_value(value: TsNode<'_>) -> Option<TsNode<'_>> {
    if is_callable_node(value) {
        return Some(value);
    }
    if value.kind() != "call_expression" {
        return None;
    }
    let args = value.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut named = args.named_children(&mut cursor);
    let first = named.next()?;
    if named.next().is_some() {
        return None; // more than one argument — a value-producing call, not a wrapper
    }
    is_callable_node(first).then_some(first)
}

fn is_callable_node(node: TsNode<'_>) -> bool {
    matches!(
        node.kind(),
        "arrow_function" | "function" | "function_expression"
    )
}

/// `exports.NAME = <fn>` or `module.exports.NAME = <fn>` at module scope → emit a
/// module-level Function node named `NAME` (CommonJS export-assignment defs, the
/// dominant handler-definition style in `require`-based backends). Returns the new
/// function's `NodeId` when one was emitted.
fn try_emit_exports_function(
    assign: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
) -> Option<NodeId> {
    let name = exports_target_name(assign, src)?;
    // Unwraps a wrapper too: `exports.register = catchAsync(async (req, res) => …)`.
    let rhs = callable_value(assign.child_by_field_name("right")?)?;
    let arity = parameter_count(rhs);
    Some(builder.emit_function(rhs, src, &name, arity, None, None))
}

/// The exported name in `exports.NAME = …` / `module.exports.NAME = …`, or `None`
/// if this assignment doesn't target an `exports` member.
fn exports_target_name(assign: TsNode<'_>, src: &str) -> Option<String> {
    let left = assign.child_by_field_name("left")?;
    if left.kind() != "member_expression" {
        return None;
    }
    let obj = left.child_by_field_name("object")?;
    let is_exports_target = match obj.kind() {
        // `exports.NAME`
        "identifier" => text(obj, src) == "exports",
        // `module.exports.NAME`
        "member_expression" => {
            obj.child_by_field_name("object").map(|o| text(o, src)).as_deref() == Some("module")
                && obj
                    .child_by_field_name("property")
                    .map(|p| text(p, src))
                    .as_deref()
                    == Some("exports")
        }
        _ => false,
    };
    if !is_exports_target {
        return None;
    }
    left.child_by_field_name("property").map(|p| text(p, src))
}

/// Barrel re-export: `module.exports.userService = require('./user.service')` — an
/// `index.js` that re-publishes sibling modules under names.
///
/// The export *is* the target module, so it's recorded as a module-scope `ModuleRef`
/// binding. A consumer's `const { userService } = require('../services')` then
/// resolves by looking up `userService` in the barrel module's scope. Without this,
/// every call through a barrel dead-ends — and barrels are the norm in this layout.
fn try_emit_exports_reexport(assign: TsNode<'_>, src: &str, builder: &mut Builder) -> bool {
    let Some(name) = exports_target_name(assign, src) else {
        return false;
    };
    let Some(rhs) = assign.child_by_field_name("right") else {
        return false;
    };
    let Some(module) = builder.require_module_of(rhs, src) else {
        return false;
    };
    let (_, sig) = builder.call_scope(None); // module scope — a barrel is top-level
    builder.type_bindings.push(TypeBinding {
        name,
        raw_type: module,
        kind: BindingKind::ModuleRef,
        in_fqcn: sig,
        range: range_of(assign),
    });
    true
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
                syntactic_callables: 0,
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

    let syntactic_callables =
        crate::generic_parse::count_callables(tree.root_node(), super::CALLABLE_KINDS);

    Ok(ParsedUnit {
        rel: rel.to_string(),
        syntactic_callables,
        nodes: builder.nodes,
        edges: builder.edges,
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

    // ── Arrow-const / higher-order function defs ──────────────────────────────

    fn fn_defs(unit: &cih_core::ParsedUnit) -> Vec<(String, String, u16)> {
        unit.parsed_file
            .defs
            .iter()
            .filter(|d| d.kind == cih_core::NodeKind::Function)
            .map(|d| {
                let arity = d
                    .id
                    .as_str()
                    .rsplit('/')
                    .next()
                    .and_then(|a| a.parse().ok())
                    .unwrap_or(0);
                (d.fqcn.clone(), d.name.clone(), arity)
            })
            .collect()
    }

    #[test]
    fn plain_arrow_consts_become_function_defs() {
        // The dominant modern JS/TS shape — and the one that produced NO node before.
        let src = "const createUser = async (body) => { return body; };\nconst getAll = function () { return []; };\n";
        let unit = parse_typescript_file("src/services/user.service.js", src).expect("parses");
        let defs = fn_defs(&unit);
        assert!(
            defs.contains(&("src/services/user.service".into(), "createUser".into(), 1)),
            "{defs:?}"
        );
        assert!(
            defs.contains(&("src/services/user.service".into(), "getAll".into(), 0)),
            "{defs:?}"
        );
    }

    #[test]
    fn higher_order_wrapped_const_becomes_function_def() {
        // `catchAsync(fn)` — an entire Express controller layer looks like this.
        // Arity comes from the INNER arrow, and the body's calls are attributed to it.
        let src = "const createUser = catchAsync(async (req, res) => { helper(req); });\n";
        let unit = parse_typescript_file("src/controllers/user.controller.js", src).expect("parses");
        let defs = fn_defs(&unit);
        assert!(
            defs.contains(&("src/controllers/user.controller".into(), "createUser".into(), 2)),
            "wrapped handler missing or wrong arity: {defs:?}"
        );
        // The inner body's call must belong to createUser, not the module.
        let site = unit
            .parsed_file
            .reference_sites
            .iter()
            .find(|s| s.name == "helper")
            .expect("inner call site missing");
        assert!(
            site.in_callable.as_str().contains("createUser"),
            "inner call attributed to {:?}, expected createUser",
            site.in_callable
        );
    }

    #[test]
    fn value_producing_call_is_not_a_function_def() {
        // Two args → a value, not a wrapper. `require()` → its arg is a string.
        // Neither may masquerade as a function definition.
        let src = "const memo = useMemo(() => compute(), [dep]);\nconst cb = useCallback(() => {}, [dep]);\nconst mod = require('./m');\nconst app = express();\n";
        let unit = parse_typescript_file("src/ui.tsx", src).expect("parses");
        let names: Vec<String> = fn_defs(&unit).into_iter().map(|(_, n, _)| n).collect();
        for n in ["memo", "cb", "mod", "app"] {
            assert!(!names.contains(&n.to_string()), "{n} wrongly emitted: {names:?}");
        }
    }

    #[test]
    fn single_param_arrow_without_parens_has_arity_one() {
        let src = "const double = x => x * 2;\n";
        let unit = parse_typescript_file("src/util.js", src).expect("parses");
        assert!(
            fn_defs(&unit).contains(&("src/util".into(), "double".into(), 1)),
            "{:?}",
            fn_defs(&unit)
        );
    }

    // ── CommonJS `require()` binding forms ────────────────────────────────────

    #[test]
    fn require_namespace_emits_module_ref_binding() {
        // `const svc = require('./service')` → ModuleRef binding whose raw_type is
        // the resolved module path, so `svc.method()` resolves against that module.
        let src = "const svc = require('./service');\nexports.h = async () => { await svc.register(x); };\n";
        let unit = parse_typescript_file("controllers/userController.js", src).expect("parses");
        let pf = &unit.parsed_file;
        let tb = pf
            .type_bindings
            .iter()
            .find(|b| b.name == "svc")
            .expect("svc binding missing");
        assert_eq!(tb.kind, cih_core::BindingKind::ModuleRef);
        assert_eq!(tb.raw_type, "controllers/service");
    }

    #[test]
    fn require_destructure_emits_static_imports() {
        // `const { a, b } = require('./m')` → static RawImport `<m>.a`, `<m>.b`.
        let src = "const { register, fetchUserBy } = require('../services/users');\n";
        let unit = parse_typescript_file("controllers/userController.js", src).expect("parses");
        let statics: Vec<&str> = unit
            .parsed_file
            .imports
            .iter()
            .filter(|i| i.is_static)
            .map(|i| i.raw.as_str())
            .collect();
        assert!(statics.contains(&"services/users.register"), "{statics:?}");
        assert!(statics.contains(&"services/users.fetchUserBy"), "{statics:?}");
    }

    #[test]
    fn require_member_capture_emits_static_import() {
        // `const extractToken = require('../utils').extractToken` → static `utils.extractToken`.
        let src = "const extractToken = require('../utils').extractToken;\n";
        let unit = parse_typescript_file("controllers/userController.js", src).expect("parses");
        assert!(
            unit.parsed_file
                .imports
                .iter()
                .any(|i| i.is_static && i.raw == "utils.extractToken"),
            "{:?}",
            unit.parsed_file.imports
        );
    }

    #[test]
    fn barrel_reexport_emits_module_ref_binding() {
        // `services/index.js` re-publishing siblings — the export IS the module.
        let src = "module.exports.userService = require('./user.service');\nexports.authService = require('./auth.service');\n";
        let unit = parse_typescript_file("src/services/index.js", src).expect("parses");
        let bindings: Vec<(&str, &str, &str)> = unit
            .parsed_file
            .type_bindings
            .iter()
            .map(|b| (b.name.as_str(), b.raw_type.as_str(), b.in_fqcn.as_str()))
            .collect();
        assert!(
            bindings.contains(&("userService", "src/services/user.service", "src/services/index")),
            "{bindings:?}"
        );
        assert!(
            bindings.contains(&("authService", "src/services/auth.service", "src/services/index")),
            "{bindings:?}"
        );
        assert!(unit
            .parsed_file
            .type_bindings
            .iter()
            .all(|b| b.kind == cih_core::BindingKind::ModuleRef));
    }

    #[test]
    fn require_destructure_emits_module_member_bindings() {
        // Destructured names are used as receivers (`userService.createUser()`), so
        // they need a binding, not just the static import that serves free calls.
        let src = "const { userService, tokenService: tok } = require('../services');\n";
        let unit = parse_typescript_file("src/controllers/user.controller.js", src).expect("parses");
        let b: Vec<(&str, &str)> = unit
            .parsed_file
            .type_bindings
            .iter()
            .filter(|b| b.kind == cih_core::BindingKind::ModuleMember)
            .map(|b| (b.name.as_str(), b.raw_type.as_str()))
            .collect();
        assert!(b.contains(&("userService", "src/services#userService")), "{b:?}");
        // Renamed: local `tok` → member `tokenService`.
        assert!(b.contains(&("tok", "src/services#tokenService")), "{b:?}");
        // The static import only makes sense when local == member.
        let statics: Vec<&str> = unit
            .parsed_file
            .imports
            .iter()
            .filter(|i| i.is_static)
            .map(|i| i.raw.as_str())
            .collect();
        assert_eq!(statics, vec!["src/services.userService"], "{statics:?}");
    }

    #[test]
    fn require_external_package_emits_no_binding() {
        // Non-relative specifiers can't map to in-repo FQCNs — no binding/import.
        let src = "const express = require('express');\nconst app = express();\n";
        let unit = parse_typescript_file("src/server.js", src).expect("parses");
        assert!(unit
            .parsed_file
            .type_bindings
            .iter()
            .all(|b| b.kind != cih_core::BindingKind::ModuleRef));
        assert!(unit
            .parsed_file
            .imports
            .iter()
            .all(|i| !i.raw.contains("express.")));
    }

    #[test]
    fn exports_assignment_emits_function_defs() {
        // CommonJS `exports.foo = () => {}` / `module.exports.bar = function(){}` →
        // module-level Function defs keyed by the module path, so require-based
        // callers have a callee node to resolve against.
        let src = "exports.register = async (req, res) => { return 1; };\nmodule.exports.fetchUserBy = function (id) { return null; };\n";
        let unit = parse_typescript_file("services/users.js", src).expect("parses");
        let fns: Vec<(&str, &str)> = unit
            .parsed_file
            .defs
            .iter()
            .filter(|d| d.kind == cih_core::NodeKind::Function)
            .map(|d| (d.fqcn.as_str(), d.name.as_str()))
            .collect();
        assert!(fns.contains(&("services/users", "register")), "{fns:?}");
        assert!(fns.contains(&("services/users", "fetchUserBy")), "{fns:?}");
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
        // A lowercase arrow const is still a function: it IS emitted as a node, it
        // just carries no React stereotype. (This assertion used to be the inverse —
        // it pinned the blind spot that left non-React arrow consts out of the graph
        // entirely.)
        assert!(
            unit.nodes.iter().any(|n| n.name == "helper"),
            "plain arrow const should be emitted as a Function node"
        );
        assert_eq!(
            stereotype_of(&unit, "helper"),
            None,
            "plain arrow const must not be labeled a React component"
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







