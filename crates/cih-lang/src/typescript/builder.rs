//! The `Builder` — accumulates the parsed graph (nodes, edges, references,
//! contracts, string constants) as the TypeScript/JS AST walker visits each
//! construct. Its `emit_*` methods append to these buffers; `parse.rs` drives the
//! walk and the framework detectors (routes, db, messaging, …) call into it.

use cih_core::{
    db_query_inline_id, db_table_id, field_id, file_id, function_id, type_id, BindingKind,
    ContractKind, ContractSite, Edge, EdgeKind, HttpWrapperDef, MessagingFramework, Node, NodeId,
    NodeKind, RawImport, Range, RefKind, ReferenceSite, RouteSource, StringConstant, SymbolDef,
    TypeBinding,
};
use tree_sitter::Node as TsNode;

use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_typescript};

use super::parse::{
    call_arity, class_extends_react_component, heritage_type_name, param_type_name, range_of,
    route_source_label, text, type_annotation_name, unquote,
};

#[derive(Default)]
pub(super) struct Builder {
    pub(super) rel: String,
    pub(super) module: String,
    pub(super) nodes: Vec<Node>,
    pub(super) edges: Vec<Edge>,
    pub(super) defs: Vec<SymbolDef>,
    pub(super) imports: Vec<RawImport>,
    pub(super) reference_sites: Vec<ReferenceSite>,
    pub(super) type_bindings: Vec<TypeBinding>,
    pub(super) contract_sites: Vec<ContractSite>,
    pub(super) string_constants: Vec<StringConstant>,
    pub(super) http_wrappers: Vec<HttpWrapperDef>,
    /// `const api = axios.create({ baseURL })` instances → optional literal
    /// baseURL, so `api.get('/x')` resolves to `<baseURL>/x` (P2 instance clients).
    pub(super) axios_instances: std::collections::HashMap<String, Option<String>>,
    /// ORM model vars → table name (`const User = mongoose.model('User', …)`,
    /// `sequelize.define('users', …)`) so `User.find()` accesses the right table (P3).
    pub(super) db_models: std::collections::HashMap<String, String>,
    /// DbTable ids already emitted this file (dedup — one table, many queries).
    pub(super) seen_db_tables: std::collections::HashSet<String>,
    /// Bull/BullMQ queue vars → queue name (`const q = new Queue('emails')`) so
    /// `q.add(...)` publishes to the right destination (P5).
    pub(super) queue_instances: std::collections::HashMap<String, String>,
}

impl Builder {
    pub(super) fn emit_class(
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
    pub(super) fn emit_heritage(
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
    pub(super) fn emit_field(
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
    pub(super) fn emit_class_fields(
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

    pub(super) fn emit_interface(&mut self, node: TsNode<'_>, src: &str, name: &str) {
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

    pub(super) fn emit_function(
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

    pub(super) fn emit_nestjs_route(
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
    pub(super) fn emit_backend_route(
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
    pub(super) fn emit_operation_route(
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
    pub(super) fn emit_operation_call(&mut self, node: TsNode<'_>, method: &str, name: &str, in_callable: NodeId) {
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
    pub(super) fn emit_db_table(&mut self, table: &str, file: &str, range: Range) {
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
    pub(super) fn emit_db_query(
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
    pub(super) fn emit_event_contract(
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
    pub(super) fn class_stereotype(
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
    pub(super) fn emit_constructor_di_refs(&mut self, class_node: TsNode<'_>, src: &str, fqn: &str) {
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
    pub(super) fn imports_pkg(&self, pkg: &str) -> bool {
        self.imports.iter().any(|imp| {
            !imp.is_static && (imp.raw == pkg || imp.raw.starts_with(&format!("{pkg}/")))
        })
    }

    /// Pick the backend framework for a verb call `<object>.<verb>(...)`, using
    /// the receiver name disambiguated by the file's imports. Express stays the
    /// default for `app`/`router`/`express` so existing behavior is preserved.
    pub(super) fn route_framework_for(&self, object: &str) -> Option<RouteSource> {
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

    pub(super) fn emit_import(&mut self, node: TsNode<'_>, src: &str) {
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
    pub(super) fn call_scope(&self, enclosing_fn: Option<&NodeId>) -> (NodeId, String) {
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

    pub(super) fn emit_call_reference(&mut self, node: TsNode<'_>, src: &str, enclosing_fn: Option<&NodeId>) {
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
    pub(super) fn emit_ctor_reference(&mut self, node: TsNode<'_>, src: &str, enclosing_fn: Option<&NodeId>) {
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
    pub(super) fn emit_param_bindings(&mut self, fn_node: TsNode<'_>, src: &str, sig: &str) {
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
