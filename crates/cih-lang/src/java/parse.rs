use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use cih_core::{
    constructor_id, field_id, file_id, method_id, type_id, BindingKind, ComplexityRecord,
    ContractKind, ContractSite, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, ParsedUnit,
    Range, RawImport, RefKind, ReferenceSite, RouteSource, SqlConstant, SqlExecutionSite,
    StringConstant, StructuralProfile, SymbolDef, TypeBinding,
};
use crate::fingerprint::{compute_body_fingerprint, normalize_leaf_token_java};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node as TsNode, QueryCursor, Tree};

use super::JavaProvider;
use crate::LanguageProvider;

#[derive(Clone, Debug)]
struct TypeContext {
    id: NodeId,
    kind: NodeKind,
    fqcn: String,
    spring_prefix: Option<String>,
    is_test: bool,
    start_byte: usize,
    end_byte: usize,
}

#[derive(Clone, Debug)]
struct CallableContext {
    id: NodeId,
    in_fqcn: String,
    start_byte: usize,
    end_byte: usize,
}

#[derive(Clone, Debug)]
struct MethodRoute {
    annotations: Vec<String>,
    http_method: &'static str,
    path: String,
    range: Range,
    source: RouteSource,
}

#[derive(Default)]
struct FileBuilder {
    file: String,
    package: Option<String>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    defs: Vec<SymbolDef>,
    imports: Vec<RawImport>,
    reference_sites: Vec<ReferenceSite>,
    type_bindings: Vec<TypeBinding>,
    contract_sites: Vec<ContractSite>,
    sql_constants: Vec<SqlConstant>,
    sql_execution_sites: Vec<SqlExecutionSite>,
    string_constants: Vec<StringConstant>,
    type_contexts: Vec<TypeContext>,
    callable_contexts: Vec<CallableContext>,
}

pub(super) fn parse_java_file(provider: &JavaProvider, rel: &str, src: &str) -> Result<ParsedUnit> {
    let tree = provider
        .parse(src)
        .with_context(|| format!("failed to parse {rel}"))?;

    let root = tree.root_node();
    let package = provider.package_of(root, src);
    let mut builder = FileBuilder {
        file: rel.to_string(),
        package,
        ..FileBuilder::default()
    };

    collect_declarations(root, src, &mut builder, None);
    collect_query_ir(provider, &tree, src, &mut builder);
    collect_heritage_references(root, src, &mut builder);
    collect_method_routes(root, src, &mut builder);
    collect_contract_sites(root, src, &mut builder);
    collect_sql_constants(root, src, &mut builder);
    collect_sql_execution_sites(root, src, &mut builder);
    collect_static_string_constants(root, src, &mut builder);
    attach_structural_profiles(&mut builder);
    normalize_builder(&mut builder);

    // Convert RawImports to ImportBindings
    let import_bindings = builder.imports.iter().map(|imp| {
        use cih_core::{ImportBinding, ImportBindingKind};
        let kind = if imp.is_wildcard {
            ImportBindingKind::Wildcard
        } else if imp.is_static {
            ImportBindingKind::StaticMember
        } else {
            ImportBindingKind::Named
        };
        let (module, imported) = if imp.is_static && !imp.is_wildcard {
            // "com.example.Util.helper" -> module="com.example.Util", imported="helper"
            if let Some((m, i)) = imp.raw.rsplit_once('.') {
                (m.to_string(), Some(i.to_string()))
            } else {
                (imp.raw.clone(), None)
            }
        } else if !imp.is_wildcard {
            // "com.example.Class" -> module="com.example", imported="Class"
            if let Some((m, i)) = imp.raw.rsplit_once('.') {
                (m.to_string(), Some(i.to_string()))
            } else {
                (imp.raw.clone(), None)
            }
        } else {
            // wildcard: module="com.example" (trim the .*)
            (imp.raw.trim_end_matches(".*").to_string(), None)
        };
        ImportBinding { module, imported, local: None, kind, range: imp.range }
    }).collect::<Vec<_>>();

    Ok(ParsedUnit {
        rel: rel.to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        import_bindings,
        parsed_file: ParsedFile {
            file: builder.file,
            language: "java".to_string(),
            package: builder.package,
            defs: builder.defs,
            imports: builder.imports,
            reference_sites: builder.reference_sites,
            type_bindings: builder.type_bindings,
            contract_sites: builder.contract_sites,
            sql_constants: builder.sql_constants,
            sql_execution_sites: builder.sql_execution_sites,
            string_constants: builder.string_constants,
        },
    })
}

fn collect_declarations(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner: Option<TypeContext>,
) {
    if let Some(kind) = type_kind(node) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let simple_name = text(name_node, src);
        if simple_name.is_empty() {
            return;
        }
        let fqcn = type_fqcn(builder.package.as_deref(), owner.as_ref(), &simple_name);
        let id = type_id(kind, &fqcn);
        let range = range_of(node);
        let owner_id = owner.as_ref().map(|owner| owner.id.clone());

        builder.nodes.push(Node {
            id: id.clone(),
            kind,
            name: simple_name.clone(),
            qualified_name: Some(fqcn.clone()),
            file: builder.file.clone(),
            range,
            props: build_class_props(node, src, &simple_name),
        });
        let stereotype = class_stereotype(node, src, &simple_name).map(|s| s.to_string());
        builder.defs.push(SymbolDef {
            id: id.clone(),
            kind,
            fqcn: fqcn.clone(),
            name: simple_name,
            owner: owner_id.clone(),
            range,
            modifiers: modifiers(node, src),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            framework_role: stereotype.clone(),
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
        });

        if let Some(parent_id) = owner_id {
            builder.edges.push(Edge {
                src: parent_id,
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "nested-type".into(),
            props: None,
            });
        } else {
            builder.edges.push(Edge {
                src: file_id(&builder.file),
                dst: id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                reason: "file-type".into(),
            props: None,
            });
        }

        let context = TypeContext {
            id,
            kind,
            fqcn,
            spring_prefix: spring_class_prefix(node, src).or_else(|| jaxrs_class_prefix(node, src)),
            is_test: stereotype.as_deref() == Some("test"),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        };
        builder.type_contexts.push(context.clone());
        walk_named_children(node, src, builder, Some(context));
        return;
    }

    if let Some(owner) = owner.as_ref() {
        match node.kind() {
            "method_declaration" => collect_method(node, src, builder, owner),
            "constructor_declaration" => collect_constructor(node, src, builder, owner),
            "field_declaration" => collect_fields(node, src, builder, owner),
            _ => {}
        }
    }

    walk_named_children(node, src, builder, owner);
}

fn walk_named_children(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner: Option<TypeContext>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_declarations(child, src, builder, owner.clone());
    }
}

fn collect_method(node: TsNode<'_>, src: &str, builder: &mut FileBuilder, owner: &TypeContext) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, src);
    if name.is_empty() {
        return;
    }
    let arity = parameter_count(node);
    let id = method_id(&owner.fqcn, &name, arity);
    let range = range_of(node);
    let return_type = return_type_name(node, src);
    let param_types = param_type_names(node, src);
    let is_test_method = owner.is_test && is_test_method(node, src);
    let is_bean = is_bean_method(node, src);

    // Gap 1: Complexity metrics
    let complexity = node
        .child_by_field_name("body")
        .map(|body| compute_complexity(body));

    // Gap 2: Body fingerprint
    let body_fingerprint = node
        .child_by_field_name("body")
        .and_then(|body| java_body_fingerprint(body));

    let props = {
        let mut obj = serde_json::Map::new();
        if is_bean {
            obj.insert("isBean".into(), serde_json::Value::Bool(true));
        }
        if is_test_method {
            obj.insert("isTest".into(), serde_json::Value::Bool(true));
        }
        if let Some(ref rt) = return_type {
            obj.insert("returnType".into(), serde_json::Value::String(rt.clone()));
        }
        if !param_types.is_empty() {
            obj.insert(
                "paramTypes".into(),
                serde_json::Value::Array(
                    param_types
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        // Gap 1: Write complexity into Node.props
        if let Some(ref cx) = complexity {
            obj.insert("cyclomatic".into(), serde_json::Value::Number(cx.cyclomatic.into()));
            obj.insert("cognitive".into(), serde_json::Value::Number(cx.cognitive.into()));
            obj.insert("loopDepth".into(), serde_json::Value::Number(cx.loop_depth.into()));
        }
        // Gap 2: Write body fingerprint into Node.props so similarity.rs can read it
        if let Some(ref fp) = body_fingerprint {
            if let Ok(v) = serde_json::to_value(fp) {
                obj.insert("bodyFingerprint".into(), v);
            }
        }
        if obj.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(obj))
        }
    };
    builder.nodes.push(Node {
        id: id.clone(),
        kind: NodeKind::Method,
        name: name.clone(),
        qualified_name: Some(format!("{}#{name}/{arity}", owner.fqcn)),
        file: builder.file.clone(),
        range,
        props,
    });
    builder.edges.push(Edge {
        src: owner.id.clone(),
        dst: id.clone(),
        kind: EdgeKind::HasMethod,
        confidence: 1.0,
        reason: "member".into(),
            props: None,
    });
    if is_test_method {
        builder.edges.push(Edge {
            src: id.clone(),
            dst: owner.id.clone(),
            kind: EdgeKind::Tests,
            confidence: 0.8,
            reason: "test-method".into(),
            props: None,
        });
    }
    builder.defs.push(SymbolDef {
        id: id.clone(),
        kind: NodeKind::Method,
        fqcn: owner.fqcn.clone(),
        name: name.clone(),
        owner: Some(owner.id.clone()),
        range,
        modifiers: modifiers(node, src),
        param_types,
        return_type,
        declared_type: None,
        framework_role: None,
        complexity,
        body_fingerprint,
        lang_meta: None,
});
    builder.callable_contexts.push(CallableContext {
        id: id.clone(),
        in_fqcn: format!("{}#{name}/{arity}", owner.fqcn),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    });
}

fn collect_constructor(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner: &TypeContext,
) {
    let arity = parameter_count(node);
    let id = constructor_id(&owner.fqcn, arity);
    let range = range_of(node);

    // Gap 1: Complexity metrics
    let complexity = node
        .child_by_field_name("body")
        .map(|body| compute_complexity(body));

    // Gap 2: Body fingerprint
    let body_fingerprint = node
        .child_by_field_name("body")
        .and_then(|body| java_body_fingerprint(body));

    let props = {
        let mut obj = serde_json::Map::new();
        if let Some(ref cx) = complexity {
            obj.insert("cyclomatic".into(), serde_json::Value::Number(cx.cyclomatic.into()));
            obj.insert("cognitive".into(), serde_json::Value::Number(cx.cognitive.into()));
            obj.insert("loopDepth".into(), serde_json::Value::Number(cx.loop_depth.into()));
        }
        // Gap 2: Write body fingerprint into Node.props so similarity.rs can read it
        if let Some(ref fp) = body_fingerprint {
            if let Ok(v) = serde_json::to_value(fp) {
                obj.insert("bodyFingerprint".into(), v);
            }
        }
        if obj.is_empty() { None } else { Some(serde_json::Value::Object(obj)) }
    };

    builder.nodes.push(Node {
        id: id.clone(),
        kind: NodeKind::Constructor,
        name: "<init>".into(),
        qualified_name: Some(format!("{}#<init>/{arity}", owner.fqcn)),
        file: builder.file.clone(),
        range,
        props,
    });
    builder.edges.push(Edge {
        src: owner.id.clone(),
        dst: id.clone(),
        kind: EdgeKind::HasMethod,
        confidence: 1.0,
        reason: "member".into(),
            props: None,
    });
    builder.defs.push(SymbolDef {
        id: id.clone(),
        kind: NodeKind::Constructor,
        fqcn: owner.fqcn.clone(),
        name: "<init>".into(),
        owner: Some(owner.id.clone()),
        range,
        modifiers: modifiers(node, src),
        param_types: param_type_names(node, src),
        return_type: None,
        declared_type: None,
        framework_role: None,
        complexity,
        body_fingerprint,
        lang_meta: None,
});
    builder.callable_contexts.push(CallableContext {
        id: id.clone(),
        in_fqcn: format!("{}#<init>/{arity}", owner.fqcn),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    });
}

fn collect_fields(node: TsNode<'_>, src: &str, builder: &mut FileBuilder, owner: &TypeContext) {
    let declared_type = node.child_by_field_name("type").map(|ty| text(ty, src));
    let is_mock_field = owner.is_test && is_mock_or_injected_field(node, src);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, src);
        if name.is_empty() {
            continue;
        }
        let id = field_id(&owner.fqcn, &name);
        let range = range_of(child);
        builder.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Field,
            name: name.clone(),
            qualified_name: Some(format!("{}#{name}", owner.fqcn)),
            file: builder.file.clone(),
            range,
            props: None,
        });
        builder.edges.push(Edge {
            src: owner.id.clone(),
            dst: id.clone(),
            kind: EdgeKind::HasField,
            confidence: 1.0,
            reason: "member".into(),
            props: None,
        });
        if is_mock_field {
            if let Some(raw_ty) = declared_type.as_deref() {
                let simple = simple_type_name(raw_ty);
                if !simple.is_empty() {
                    builder.edges.push(Edge {
                        src: owner.id.clone(),
                        dst: NodeId::new(format!("Class:{simple}")),
                        kind: EdgeKind::Tests,
                        confidence: 0.7,
                        reason: "mock-bean".into(),
            props: None,
                    });
                }
            }
        }
        builder.defs.push(SymbolDef {
            id,
            kind: NodeKind::Field,
            fqcn: owner.fqcn.clone(),
            name,
            owner: Some(owner.id.clone()),
            range,
            modifiers: modifiers(node, src),
            param_types: Vec::new(),
            return_type: None,
            declared_type: declared_type.clone(),
            framework_role: None,
            complexity: None,
            body_fingerprint: None,
        lang_meta: None,
        });
    }
}

fn collect_query_ir(provider: &JavaProvider, tree: &Tree, src: &str, builder: &mut FileBuilder) {
    let mut cursor = QueryCursor::new();
    let query = provider.scope_query();
    let capture_names = query.capture_names();
    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());

    while let Some(query_match) = matches.next() {
        let mut captures: BTreeMap<String, TsNode<'_>> = BTreeMap::new();
        for capture in query_match.captures {
            let name = capture_names[capture.index as usize].to_string();
            captures.entry(name).or_insert(capture.node);
        }

        if let Some(import_node) = captures.get("import.statement").copied() {
            if import_node.kind() == "import_declaration" {
                if let Some(import) = parse_import(import_node, src) {
                    builder.imports.push(import);
                }
            }
            continue;
        }

        if let Some(binding) = type_binding(&captures, src, builder) {
            builder.type_bindings.push(binding);
            continue;
        }

        if let Some(site) = reference_site(&captures, src, builder) {
            builder.reference_sites.push(site);
        }
    }
}

fn reference_site(
    captures: &BTreeMap<String, TsNode<'_>>,
    src: &str,
    builder: &FileBuilder,
) -> Option<ReferenceSite> {
    let anchor = reference_anchor(captures)?;
    let name_node = captures
        .get("reference.name")
        .copied()
        .unwrap_or(anchor.node);
    let name = text(name_node, src);
    if name.is_empty() {
        return None;
    }

    if anchor.kind == RefKind::Call
        && anchor.tag == "reference.call.free"
        && anchor.node.child_by_field_name("object").is_some()
    {
        return None;
    }
    if anchor.kind == RefKind::FieldRead && !should_emit_field_read(anchor.node) {
        return None;
    }

    let receiver = captures
        .get("reference.receiver")
        .map(|node| text(*node, src))
        .filter(|value| !value.is_empty());
    let arity = match anchor.kind {
        RefKind::Call | RefKind::Ctor => call_arity(anchor.node),
        _ => None,
    };
    let in_fqcn = context_for(anchor.node.start_byte(), builder).unwrap_or_default();
    let in_callable = callable_id_for(anchor.node.start_byte(), builder);

    // Capture arg texts for Call sites (Gap 3)
    let arg_texts = if anchor.kind == RefKind::Call {
        capture_arg_texts(anchor.node, src)
    } else {
        Vec::new()
    };

    Some(ReferenceSite {
        name,
        receiver,
        kind: anchor.kind,
        arity,
        range: range_of(name_node),
        in_fqcn,
        in_callable,
        arg_texts,
})
}

#[derive(Clone, Copy)]
struct ReferenceAnchor<'a> {
    tag: &'a str,
    node: TsNode<'a>,
    kind: RefKind,
}

fn reference_anchor<'a>(captures: &'a BTreeMap<String, TsNode<'a>>) -> Option<ReferenceAnchor<'a>> {
    if let Some(node) = captures.get("reference.call.constructor").copied() {
        return Some(ReferenceAnchor {
            tag: "reference.call.constructor",
            node,
            kind: RefKind::Ctor,
        });
    }
    if let Some((tag, node)) = captures
        .iter()
        .find(|(tag, _)| tag.starts_with("reference.call."))
    {
        return Some(ReferenceAnchor {
            tag,
            node: *node,
            kind: RefKind::Call,
        });
    }
    if let Some(node) = captures.get("reference.write.member").copied() {
        return Some(ReferenceAnchor {
            tag: "reference.write.member",
            node,
            kind: RefKind::FieldWrite,
        });
    }
    if let Some(node) = captures.get("reference.read.member").copied() {
        return Some(ReferenceAnchor {
            tag: "reference.read.member",
            node,
            kind: RefKind::FieldRead,
        });
    }
    None
}

fn type_binding(
    captures: &BTreeMap<String, TsNode<'_>>,
    src: &str,
    builder: &FileBuilder,
) -> Option<TypeBinding> {
    let (anchor_tag, anchor_node) = captures.iter().find(|(key, _)| {
        let key = key.as_str();
        key.starts_with("type-binding.") && key != "type-binding.type" && key != "type-binding.name"
    })?;
    let type_node = captures.get("type-binding.type")?;
    let name_node = captures.get("type-binding.name")?;
    let raw_type = text(*type_node, src);
    let name = text(*name_node, src);
    if raw_type.is_empty() || name.is_empty() {
        return None;
    }
    Some(TypeBinding {
        name,
        raw_type,
        kind: binding_kind(anchor_tag.as_str(), *anchor_node),
        in_fqcn: context_for(anchor_node.start_byte(), builder).unwrap_or_default(),
        range: range_of(*name_node),
    })
}

fn binding_kind(tag: &str, anchor: TsNode<'_>) -> BindingKind {
    match tag {
        "type-binding.parameter" => BindingKind::Param,
        "type-binding.call-result" => BindingKind::CallResult,
        "type-binding.alias" => BindingKind::Alias,
        "type-binding.constructor" => BindingKind::Local,
        "type-binding.return" => BindingKind::Return,
        "type-binding.pattern" => BindingKind::Pattern,
        "type-binding.annotation" => match anchor.kind() {
            "field_declaration" => BindingKind::Field,
            _ => BindingKind::Local,
        },
        _ => BindingKind::Local,
    }
}

fn collect_heritage_references(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    match node.kind() {
        "class_declaration" => {
            let (in_fqcn, owner_id) = heritage_owner(node, builder);
            if let Some(superclass) = node.child_by_field_name("superclass") {
                emit_heritage_from_children(
                    superclass,
                    src,
                    builder,
                    RefKind::Extends,
                    &in_fqcn,
                    &owner_id,
                );
            }
            if let Some(interfaces) = node.child_by_field_name("interfaces") {
                emit_heritage_type_list(
                    interfaces,
                    src,
                    builder,
                    RefKind::Implements,
                    &in_fqcn,
                    &owner_id,
                );
            }
        }
        "interface_declaration" => {
            let (in_fqcn, owner_id) = heritage_owner(node, builder);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "extends_interfaces" {
                    emit_heritage_type_list(
                        child,
                        src,
                        builder,
                        RefKind::Extends,
                        &in_fqcn,
                        &owner_id,
                    );
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_heritage_references(child, src, builder);
    }
}

fn heritage_owner(node: TsNode<'_>, builder: &FileBuilder) -> (String, NodeId) {
    match type_context_at(node.start_byte(), builder) {
        Some(ctx) => (ctx.fqcn.clone(), ctx.id.clone()),
        None => (String::new(), file_id(&builder.file)),
    }
}

fn emit_heritage_type_list(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    kind: RefKind,
    in_fqcn: &str,
    owner_id: &NodeId,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_list" {
            emit_heritage_from_children(child, src, builder, kind, in_fqcn, owner_id);
        } else if let Some(name_node) = base_name_node(child) {
            emit_heritage_reference(name_node, src, builder, kind, in_fqcn, owner_id);
        }
    }
}

fn emit_heritage_from_children(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    kind: RefKind,
    in_fqcn: &str,
    owner_id: &NodeId,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name_node) = base_name_node(child) {
            emit_heritage_reference(name_node, src, builder, kind, in_fqcn, owner_id);
        }
    }
}

fn emit_heritage_reference(
    name_node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    kind: RefKind,
    in_fqcn: &str,
    owner_id: &NodeId,
) {
    let name = text(name_node, src);
    if name.is_empty() {
        return;
    }
    builder.reference_sites.push(ReferenceSite {
        name,
        receiver: None,
        kind,
        arity: None,
        range: range_of(name_node),
        in_fqcn: in_fqcn.to_string(),
        in_callable: owner_id.clone(),
        arg_texts: Vec::new(),
    });
}

fn collect_method_routes(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    if node.kind() == "method_declaration" {
        emit_method_routes_for_method(node, src, builder);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_method_routes(child, src, builder);
    }
}

fn emit_method_routes_for_method(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let routes = method_routes(node, src);
    if routes.is_empty() {
        return;
    }

    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    // Class-level prefix may come from Spring's @RequestMapping or JAX-RS's @Path.
    let prefix = type_context_at(node.start_byte(), builder)
        .and_then(|ctx| ctx.spring_prefix.clone())
        .filter(|p| !p.is_empty())
        .unwrap_or_default();

    for route in routes {
        let path = normalize_route_path(&route.path, &prefix);
        let name = format!("{} {path}", route.http_method);
        let route_id = NodeId::new(format!("Route:{name}"));
        let reason = match route.source {
            RouteSource::SpringMvc => format!(
                "spring-{}",
                route.annotations.first().map(String::as_str).unwrap_or("")
            ),
            RouteSource::JaxRs => format!("jaxrs-{}", route.http_method),
            _ => format!("{:?}-{}", route.source, route.http_method),
        };
        builder.nodes.push(Node {
            id: route_id.clone(),
            kind: NodeKind::Route,
            name: name.clone(),
            qualified_name: Some(name),
            file: builder.file.clone(),
            range: route.range,
            props: Some(serde_json::json!({
                "httpMethod": route.http_method,
                "path": path,
                "route_annotations": route.annotations,
                "source": route.source,
                "handler": callable.in_fqcn,
            })),
        });
        builder.edges.push(Edge {
            src: callable.id.clone(),
            dst: route_id,
            kind: EdgeKind::HandlesRoute,
            confidence: 1.0,
            reason,
            props: None,
        });
    }
}

fn collect_contract_sites(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    match node.kind() {
        "interface_declaration" => emit_feign_contracts(node, src, builder),
        "method_declaration" => emit_listener_contracts(node, src, builder),
        "method_invocation" => emit_invocation_contract(node, src, builder),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_contract_sites(child, src, builder);
    }
}

fn emit_feign_contracts(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let Some(feign) = annotations(node)
        .into_iter()
        .find(|ann| annotation_name(*ann, src).as_deref() == Some("FeignClient"))
    else {
        return;
    };
    let base = annotation_string_values(feign, src, &["url", "path", "value"])
        .into_iter()
        .next();

    for method in method_declarations(node) {
        let Some(callable) = callable_context_at(method.start_byte(), builder).cloned() else {
            continue;
        };
        for route in spring_method_routes_inner(method, src) {
            let url = if let Some(base) = base.as_deref().filter(|base| base.starts_with('/')) {
                normalize_route_path(&route.path, base)
            } else {
                normalize_external_url(&route.path)
            };
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::HttpClientProxy,
                url_template: Some(url),
                topic: None,
                http_method: Some(route.http_method.to_string()),
                in_callable: callable.id.clone(),
                range: route.range,
            });
        }
    }
}

fn emit_listener_contracts(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    for annotation in annotations(node) {
        match annotation_name(annotation, src).as_deref() {
            Some("KafkaListener") => {
                for topic in
                    annotation_string_values(annotation, src, &["topics", "topic", "value"])
                {
                    builder.contract_sites.push(ContractSite {
                        kind: ContractKind::EventListen,
                        url_template: None,
                        topic: Some(topic),
                        http_method: None,
                        in_callable: callable.id.clone(),
                        range: range_of(annotation),
                    });
                }
            }
            Some("EventListener") => {
                if let Some(topic) = param_type_names(node, src).into_iter().next() {
                    builder.contract_sites.push(ContractSite {
                        kind: ContractKind::EventListen,
                        url_template: None,
                        topic: Some(base_type_simple(&topic)),
                        http_method: None,
                        in_callable: callable.id.clone(),
                        range: range_of(annotation),
                    });
                }
            }
            _ => {}
        }
    }
}

fn emit_invocation_contract(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let method = text(name_node, src);
    let Some(callable) = callable_context_at(node.start_byte(), builder).cloned() else {
        return;
    };
    let receiver = node
        .child_by_field_name("object")
        .map(|object| text(object, src))
        .unwrap_or_default();

    if let Some(http_method) = rest_template_http_method(&method) {
        if receiver_has_type(builder, &callable.in_fqcn, &receiver, "RestTemplate") {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::HttpCall,
                url_template: first_string_argument(node, src)
                    .map(|url| normalize_external_url(&url)),
                topic: None,
                http_method: Some(http_method.to_string()),
                in_callable: callable.id,
                range: range_of(node),
            });
        }
        return;
    }

    if method == "uri" {
        if let Some(http_method) = infer_webclient_http_method(&receiver) {
            if root_receiver_has_type(builder, &callable.in_fqcn, &receiver, "WebClient") {
                builder.contract_sites.push(ContractSite {
                    kind: ContractKind::HttpCall,
                    url_template: first_string_argument(node, src)
                        .map(|url| normalize_external_url(&url)),
                    topic: None,
                    http_method: Some(http_method.to_string()),
                    in_callable: callable.id,
                    range: range_of(node),
                });
            }
        }
        return;
    }

    if method == "send" && receiver_has_type(builder, &callable.in_fqcn, &receiver, "KafkaTemplate")
    {
        if let Some(topic) = first_string_argument(node, src) {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic: Some(topic),
                http_method: None,
                in_callable: callable.id,
                range: range_of(node),
            });
        }
        return;
    }

    if method == "publishEvent"
        && receiver_has_type(
            builder,
            &callable.in_fqcn,
            &receiver,
            "ApplicationEventPublisher",
        )
    {
        if let Some(topic) = first_constructor_argument_type(node, src) {
            builder.contract_sites.push(ContractSite {
                kind: ContractKind::EventPublish,
                url_template: None,
                topic: Some(topic),
                http_method: None,
                in_callable: callable.id,
                range: range_of(node),
            });
        }
    }
}

// ── Gap 1: Complexity metrics ────────────────────────────────────────────────

/// Compute cyclomatic, cognitive, and loop depth for a method/constructor body.
fn compute_complexity(body: TsNode<'_>) -> ComplexityRecord {
    let mut cyclomatic: u16 = 1; // base
    let mut cognitive: u16 = 0;
    let mut loop_depth: u8 = 0;
    let mut counts = ControlFlowCounts::default();
    compute_complexity_inner(body, 0, 0, &mut cyclomatic, &mut cognitive, &mut loop_depth, &mut counts);
    ComplexityRecord {
        provider: "java".to_string(),
        cyclomatic,
        cognitive,
        loop_depth,
        is_recursive: false, // set later in propagation pass
        if_count: counts.if_count,
        for_count: counts.for_count,
        while_count: counts.while_count,
        switch_count: counts.switch_count,
        try_count: counts.try_count,
        return_count: counts.return_count,
        throw_count: counts.throw_count,
    }
}

#[derive(Default)]
struct ControlFlowCounts {
    if_count: u16,
    for_count: u16,
    while_count: u16,
    switch_count: u16,
    try_count: u16,
    return_count: u16,
    throw_count: u16,
}

#[allow(clippy::too_many_arguments)]
fn compute_complexity_inner(
    node: TsNode<'_>,
    nesting: u16,
    loop_nesting: u8,
    cyclomatic: &mut u16,
    cognitive: &mut u16,
    max_loop_depth: &mut u8,
    counts: &mut ControlFlowCounts,
) {
    let kind = node.kind();

    let (new_nesting, new_loop_nesting) = match kind {
        "if_statement" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.if_count = counts.if_count.saturating_add(1);
            (nesting + 1, loop_nesting)
        }
        "while_statement" | "do_statement" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.while_count = counts.while_count.saturating_add(1);
            let new_ld = loop_nesting + 1;
            if new_ld > *max_loop_depth {
                *max_loop_depth = new_ld;
            }
            (nesting + 1, new_ld)
        }
        "for_statement" | "enhanced_for_statement" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.for_count = counts.for_count.saturating_add(1);
            let new_ld = loop_nesting + 1;
            if new_ld > *max_loop_depth {
                *max_loop_depth = new_ld;
            }
            (nesting + 1, new_ld)
        }
        "switch_expression" | "switch_statement" => {
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.switch_count = counts.switch_count.saturating_add(1);
            (nesting + 1, loop_nesting)
        }
        "switch_label" => {
            // count each case
            *cyclomatic = cyclomatic.saturating_add(1);
            (nesting, loop_nesting)
        }
        "catch_clause" => {
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            (nesting + 1, loop_nesting)
        }
        "try_statement" => {
            *cognitive = cognitive.saturating_add(1 + nesting);
            counts.try_count = counts.try_count.saturating_add(1);
            (nesting + 1, loop_nesting)
        }
        "return_statement" => {
            counts.return_count = counts.return_count.saturating_add(1);
            (nesting, loop_nesting)
        }
        "throw_statement" => {
            counts.throw_count = counts.throw_count.saturating_add(1);
            (nesting, loop_nesting)
        }
        "conditional_expression" => {
            // ternary
            *cyclomatic = cyclomatic.saturating_add(1);
            *cognitive = cognitive.saturating_add(1 + nesting);
            (nesting, loop_nesting)
        }
        "binary_expression" => {
            // && and || logical operators
            (nesting, loop_nesting)
        }
        "else" => {
            *cognitive = cognitive.saturating_add(1);
            (nesting, loop_nesting)
        }
        "break_statement" | "continue_statement" => {
            // labelled break/continue
            if node.child_by_field_name("label").is_some() {
                *cognitive = cognitive.saturating_add(1);
            }
            (nesting, loop_nesting)
        }
        "lambda_expression" => {
            // lambdas reset nesting for cognitive
            (0, loop_nesting)
        }
        _ => (nesting, loop_nesting),
    };

    // Count && and || in binary_expression
    if kind == "binary_expression" {
        if let Some(op) = node.child_by_field_name("operator") {
            let op_text = op.kind();
            if op_text == "&&" || op_text == "||" {
                *cyclomatic = cyclomatic.saturating_add(1);
                *cognitive = cognitive.saturating_add(1);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        compute_complexity_inner(
            child,
            new_nesting,
            new_loop_nesting,
            cyclomatic,
            cognitive,
            max_loop_depth,
            counts,
        );
    }
}

// ── Body fingerprint delegated to crate::fingerprint ─────────────────────────

fn java_body_fingerprint(body: TsNode<'_>) -> Option<cih_core::BodyFingerprint> {
    compute_body_fingerprint(body, "java", normalize_leaf_token_java)
}

fn collect_sql_constants(root: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    collect_sql_constants_in(root, src, builder, None);
}

/// Collect ALL `static final String` fields (not just SQL-named ones) into `string_constants`.
fn collect_static_string_constants(root: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    collect_static_string_constants_in(root, src, builder, None);
}

fn collect_static_string_constants_in(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner_fqcn: Option<&str>,
) {
    match node.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let fqcn = type_context_at(node.start_byte() + 1, builder).map(|t| t.fqcn.clone());
            let effective = fqcn.as_deref().or(owner_fqcn);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_static_string_constants_in(child, src, builder, effective);
            }
            return;
        }
        "field_declaration" => {
            if let Some(owner) = owner_fqcn {
                if let Some(sc) = try_extract_string_constant(node, src, owner) {
                    builder.string_constants.push(sc);
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_static_string_constants_in(child, src, builder, owner_fqcn);
    }
}

fn try_extract_string_constant(
    node: TsNode<'_>,
    src: &str,
    owner_fqcn: &str,
) -> Option<StringConstant> {
    let mods = modifiers(node, src);
    if !mods.iter().any(|m| m == "static") || !mods.iter().any(|m| m == "final") {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    if text(type_node, src) != "String" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let name_node = child.child_by_field_name("name")?;
        let const_name = text(name_node, src);
        if const_name.is_empty() {
            continue;
        }
        let Some(value_node) = child.child_by_field_name("value") else {
            continue;
        };
        let (value, dynamic) = fold_string_init(value_node, src);
        if value.is_empty() && !dynamic {
            continue;
        }
        return Some(StringConstant {
            const_name,
            owner_fqcn: owner_fqcn.to_string(),
            value,
            dynamic,
            range: range_of(node),
        });
    }
    None
}

fn collect_sql_constants_in(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner_fqcn: Option<&str>,
) {
    match node.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let fqcn = type_context_at(node.start_byte() + 1, builder).map(|t| t.fqcn.clone());
            let effective = fqcn.as_deref().or(owner_fqcn);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_sql_constants_in(child, src, builder, effective);
            }
            return;
        }
        "field_declaration" => {
            if let Some(owner) = owner_fqcn {
                if let Some(constant) = try_extract_sql_constant(node, src, owner) {
                    builder.sql_constants.push(constant);
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_sql_constants_in(child, src, builder, owner_fqcn);
    }
}

fn try_extract_sql_constant(node: TsNode<'_>, src: &str, owner_fqcn: &str) -> Option<SqlConstant> {
    let mods = modifiers(node, src);
    if !mods.iter().any(|m| m == "static") || !mods.iter().any(|m| m == "final") {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    if text(type_node, src) != "String" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let name_node = child.child_by_field_name("name")?;
        let const_name = text(name_node, src);
        if const_name.is_empty()
            || !const_name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            continue;
        }
        let Some(value_node) = child.child_by_field_name("value") else {
            continue;
        };
        let (sql_text, dynamic) = fold_string_init(value_node, src);
        if sql_text.is_empty() && !dynamic {
            continue;
        }
        return Some(SqlConstant {
            const_name,
            owner_fqcn: owner_fqcn.to_string(),
            sql_text,
            dynamic,
            range: range_of(node),
        });
    }
    None
}

fn fold_string_init(node: TsNode<'_>, src: &str) -> (String, bool) {
    match node.kind() {
        "string_literal" => {
            let raw = text(node, src);
            let inner = if raw.len() >= 2 {
                &raw[1..raw.len() - 1]
            } else {
                ""
            };
            let unescaped = inner
                .replace("\\n", " ")
                .replace("\\r", " ")
                .replace("\\t", " ")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            (unescaped, false)
        }
        "binary_expression" => {
            if node
                .child_by_field_name("operator")
                .map(|op| text(op, src))
                .as_deref()
                != Some("+")
            {
                return (String::new(), true);
            }
            let left = node
                .child_by_field_name("left")
                .map(|n| fold_string_init(n, src))
                .unwrap_or_default();
            let right = node
                .child_by_field_name("right")
                .map(|n| fold_string_init(n, src))
                .unwrap_or_default();
            (format!("{}{}", left.0, right.0), left.1 || right.1)
        }
        "parenthesized_expression" => {
            if let Some(inner) = node.named_child(0) {
                fold_string_init(inner, src)
            } else {
                (String::new(), true)
            }
        }
        _ => (String::new(), true),
    }
}

fn collect_sql_execution_sites(root: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    collect_sql_execution_sites_in(root, src, builder);
}

fn collect_sql_execution_sites_in(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    if node.kind() == "method_invocation" {
        if let Some(site) = try_extract_execution_site(node, src, builder) {
            builder.sql_execution_sites.push(site);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_sql_execution_sites_in(child, src, builder);
    }
}

const DBUTIL_METHODS: &[&str] = &["prepareStatement", "executeQuery", "executeUpdate"];
const JDBC_TEMPLATE_METHODS: &[&str] = &[
    "query",
    "update",
    "queryForObject",
    "queryForList",
    "queryForMap",
    "batchUpdate",
];

fn try_extract_execution_site(
    node: TsNode<'_>,
    src: &str,
    builder: &FileBuilder,
) -> Option<SqlExecutionSite> {
    let method_name_node = node.child_by_field_name("name")?;
    let method = text(method_name_node, src);
    let range = range_of(node);
    let in_callable = callable_id_for(node.start_byte(), builder);
    let in_fqcn = context_for(node.start_byte(), builder).unwrap_or_default();

    let object = node.child_by_field_name("object")?;
    let receiver = text(object, src);

    if receiver == "DBUtil" && DBUTIL_METHODS.contains(&method.as_str()) {
        let const_ref = nth_identifier_argument(node, src, 1);
        if const_ref.is_some() || method == "prepareStatement" {
            return Some(SqlExecutionSite {
                api_name: method,
                const_ref,
                inline_sql: None,
                in_callable,
                range,
            });
        }
    }

    if JDBC_TEMPLATE_METHODS.contains(&method.as_str())
        && receiver_has_type(builder, &in_fqcn, &receiver, "JdbcTemplate")
    {
        let (const_ref, inline_sql) = first_sql_argument(node, src);
        if const_ref.is_some() || inline_sql.is_some() {
            return Some(SqlExecutionSite {
                api_name: method,
                const_ref,
                inline_sql,
                in_callable,
                range,
            });
        }
    }

    None
}

fn nth_identifier_argument(node: TsNode<'_>, src: &str, n: usize) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut count = 0;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            if count == n {
                let name = text(child, src);
                if !name.is_empty() {
                    return Some(name);
                }
            }
            count += 1;
        } else if !matches!(child.kind(), "line_comment" | "block_comment") && count <= n {
            count += 1;
        }
    }
    None
}

fn first_sql_argument(node: TsNode<'_>, src: &str) -> (Option<String>, Option<String>) {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return (None, None);
    };
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => return (Some(text(child, src)), None),
            "string_literal" => {
                return (None, unquote_spring_literal(&text(child, src)));
            }
            _ => continue,
        }
    }
    (None, None)
}

fn method_declarations(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let mut out = Vec::new();
    collect_method_declarations(node, &mut out);
    out
}

fn collect_method_declarations<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if node.kind() == "method_declaration" {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_method_declarations(child, out);
    }
}

fn annotation_string_values(node: TsNode<'_>, src: &str, keys: &[&str]) -> Vec<String> {
    let Some(arguments) = first_named_child(node, "annotation_argument_list") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "string_literal" => {
                if keys.contains(&"value") {
                    if let Some(value) = unquote_spring_literal(&text(child, src)) {
                        out.push(value);
                    }
                }
            }
            "element_value_array_initializer" | "array_initializer" => {
                if keys.contains(&"value") {
                    for string_node in string_literals(child) {
                        if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                            out.push(value);
                        }
                    }
                }
            }
            "element_value_pair" => {
                let key = child
                    .child_by_field_name("key")
                    .or_else(|| first_named_child(child, "identifier"))
                    .map(|node| text(node, src));
                if !key
                    .as_deref()
                    .is_some_and(|key| keys.iter().any(|candidate| candidate == &key))
                {
                    continue;
                }
                for string_node in string_literals(child) {
                    if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                        out.push(value);
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn rest_template_http_method(method: &str) -> Option<&'static str> {
    match method {
        "getForObject" | "getForEntity" => Some("GET"),
        "postForObject" | "postForEntity" | "postForLocation" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patchForObject" => Some("PATCH"),
        "exchange" => None,
        _ => None,
    }
}

fn infer_webclient_http_method(receiver: &str) -> Option<&'static str> {
    for (needle, method) in [
        (".get()", "GET"),
        (".post()", "POST"),
        (".put()", "PUT"),
        (".delete()", "DELETE"),
        (".patch()", "PATCH"),
    ] {
        if receiver.contains(needle) {
            return Some(method);
        }
    }
    None
}

fn first_string_argument(node: TsNode<'_>, src: &str) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "string_literal" {
            return unquote_spring_literal(&text(child, src));
        }
    }
    None
}

fn first_constructor_argument_type(node: TsNode<'_>, src: &str) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "object_creation_expression" {
            let raw = child
                .child_by_field_name("type")
                .or_else(|| child.named_child(0))
                .map(|ty| text(ty, src))?;
            return Some(base_type_simple(&raw));
        }
    }
    None
}

fn receiver_has_type(builder: &FileBuilder, in_fqcn: &str, receiver: &str, expected: &str) -> bool {
    let receiver = receiver.trim();
    if receiver.is_empty() {
        return false;
    }
    let candidate = receiver.rsplit('.').next().unwrap_or(receiver);
    binding_has_type(builder, in_fqcn, candidate.trim_end_matches("()"), expected)
}

fn root_receiver_has_type(
    builder: &FileBuilder,
    in_fqcn: &str,
    receiver: &str,
    expected: &str,
) -> bool {
    let root = receiver
        .split('.')
        .next()
        .unwrap_or(receiver)
        .trim()
        .trim_end_matches("()");
    binding_has_type(builder, in_fqcn, root, expected)
}

fn binding_has_type(builder: &FileBuilder, in_fqcn: &str, name: &str, expected: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let owner = class_of_signature(in_fqcn);
    builder.type_bindings.iter().any(|binding| {
        binding.name == name
            && (binding.in_fqcn == in_fqcn || binding.in_fqcn == owner)
            && base_type_simple(&binding.raw_type) == expected
    })
}

fn class_of_signature(in_fqcn: &str) -> &str {
    in_fqcn.split('#').next().unwrap_or(in_fqcn)
}

fn base_type_simple(raw: &str) -> String {
    raw.split('<')
        .next()
        .unwrap_or(raw)
        .replace("[]", "")
        .rsplit('.')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

fn normalize_external_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
    {
        return rest
            .find('/')
            .map(|idx| collapse_slashes(&rest[idx..]))
            .unwrap_or_else(|| "/".to_string());
    }
    if trimmed.starts_with('/') {
        collapse_slashes(trimmed)
    } else {
        trimmed.to_string()
    }
}

fn spring_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("RequestMapping"))
        .and_then(|annotation| first_route_value(annotation, src))
}

fn jaxrs_class_prefix(node: TsNode<'_>, src: &str) -> Option<String> {
    annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("Path"))
        .and_then(|annotation| first_route_value(annotation, src))
}

/// Collect routes from both Spring MVC and JAX-RS annotations on a method,
/// deduplicated by (http_method, path) and sorted for stable output.
fn method_routes(node: TsNode<'_>, src: &str) -> Vec<MethodRoute> {
    let mut routes = spring_method_routes_inner(node, src);
    routes.extend(jaxrs_method_routes_inner(node, src));
    routes.sort_by(|a, b| {
        a.http_method
            .cmp(b.http_method)
            .then(a.path.cmp(&b.path))
    });
    routes.dedup_by(|a, b| a.http_method == b.http_method && a.path == b.path);
    routes
}

fn spring_method_routes_inner(node: TsNode<'_>, src: &str) -> Vec<MethodRoute> {
    let mut routes = Vec::new();
    for annotation in annotations(node) {
        let Some(annotation_name) = annotation_name(annotation, src) else {
            continue;
        };
        let Some(http_method) = spring_http_method(&annotation_name) else {
            continue;
        };
        let paths = route_values(annotation, src);
        if paths.is_empty() {
            // bare @GetMapping / @DeleteMapping with no path → inherits class-level prefix
            routes.push(MethodRoute {
                annotations: vec![annotation_name.clone()],
                http_method,
                path: String::new(),
                range: range_of(annotation),
                source: RouteSource::SpringMvc,
            });
        } else {
            for path in paths {
                routes.push(MethodRoute {
                    annotations: vec![annotation_name.clone()],
                    http_method,
                    path,
                    range: range_of(annotation),
                    source: RouteSource::SpringMvc,
                });
            }
        }
    }
    routes
}

/// JAX-RS routes: HTTP verb comes from `@GET`/`@POST`/... and the method-level
/// path (if any) from `@Path`. Class-level `@Path` is applied separately as the
/// route prefix in [`emit_method_routes_for_method`].
fn jaxrs_method_routes_inner(node: TsNode<'_>, src: &str) -> Vec<MethodRoute> {
    let verb = annotations(node)
        .into_iter()
        .find_map(|annotation| {
            annotation_name(annotation, src)
                .as_deref()
                .and_then(jaxrs_http_method)
                .map(|method| (method, annotation))
        });
    let Some((http_method, verb_annotation)) = verb else {
        return Vec::new();
    };

    let path_annotation = annotations(node)
        .into_iter()
        .find(|annotation| annotation_name(*annotation, src).as_deref() == Some("Path"));
    let paths = path_annotation
        .map(|annotation| route_values(annotation, src))
        .unwrap_or_default();
    let verb_name = annotation_name(verb_annotation, src).unwrap_or_else(|| http_method.to_string());
    let mut annotation_names = vec![verb_name];
    if path_annotation.is_some() {
        annotation_names.push("Path".to_string());
    }
    annotation_names.sort();

    let range = range_of(verb_annotation);
    if paths.is_empty() {
        // No @Path on the method → inherits the class-level @Path prefix.
        vec![MethodRoute {
            annotations: annotation_names,
            http_method,
            path: String::new(),
            range,
            source: RouteSource::JaxRs,
        }]
    } else {
        paths
            .into_iter()
            .map(|path| MethodRoute {
                annotations: annotation_names.clone(),
                http_method,
                path,
                range,
                source: RouteSource::JaxRs,
            })
            .collect()
    }
}

fn annotations(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let Some(modifiers) = first_named_child(node, "modifiers") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_annotations(modifiers, &mut out);
    out
}

fn collect_annotations<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if matches!(node.kind(), "annotation" | "marker_annotation") {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_annotations(child, out);
    }
}

fn annotation_name(node: TsNode<'_>, src: &str) -> Option<String> {
    node.child_by_field_name("name")
        .or_else(|| first_named_child(node, "identifier"))
        .map(|name| text(name, src))
        .filter(|name| !name.is_empty())
}

fn spring_http_method(annotation: &str) -> Option<&'static str> {
    match annotation {
        "GetMapping" => Some("GET"),
        "PostMapping" => Some("POST"),
        "PutMapping" => Some("PUT"),
        "DeleteMapping" => Some("DELETE"),
        "PatchMapping" => Some("PATCH"),
        _ => None,
    }
}

fn jaxrs_http_method(annotation: &str) -> Option<&'static str> {
    match annotation {
        "GET" => Some("GET"),
        "POST" => Some("POST"),
        "PUT" => Some("PUT"),
        "DELETE" => Some("DELETE"),
        "PATCH" => Some("PATCH"),
        "HEAD" => Some("HEAD"),
        "OPTIONS" => Some("OPTIONS"),
        _ => None,
    }
}

fn first_route_value(annotation: TsNode<'_>, src: &str) -> Option<String> {
    route_values(annotation, src).into_iter().next()
}

fn route_values(annotation: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(arguments) = first_named_child(annotation, "annotation_argument_list") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "string_literal" => {
                if let Some(value) = unquote_spring_literal(&text(child, src)) {
                    out.push(value);
                }
            }
            "element_value_array_initializer" | "array_initializer" => {
                for string_node in string_literals(child) {
                    if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                        out.push(value);
                    }
                }
            }
            "element_value_pair" => {
                let key = child
                    .child_by_field_name("key")
                    .or_else(|| first_named_child(child, "identifier"))
                    .map(|node| text(node, src));
                if !is_route_member_key(key.as_deref()) {
                    continue;
                }
                for string_node in string_literals(child) {
                    if let Some(value) = unquote_spring_literal(&text(string_node, src)) {
                        out.push(value);
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn string_literals(node: TsNode<'_>) -> Vec<TsNode<'_>> {
    let mut out = Vec::new();
    collect_string_literals(node, &mut out);
    out
}

fn collect_string_literals<'a>(node: TsNode<'a>, out: &mut Vec<TsNode<'a>>) {
    if node.kind() == "string_literal" {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_string_literals(child, out);
    }
}

fn is_route_member_key(key: Option<&str>) -> bool {
    key.is_none_or(|key| key == "path" || key == "value")
}

fn unquote_spring_literal(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if (raw.starts_with("\"\"\"") && raw.ends_with("\"\"\""))
        || (raw.starts_with("'''") && raw.ends_with("'''"))
    {
        return Some(raw[3..raw.len().saturating_sub(3)].to_string());
    }

    let mut chars = raw.chars();
    let first = chars.next()?;
    let last = raw.chars().next_back()?;
    if matches!(first, '"' | '\'' | '`') && first == last && raw.len() >= 2 {
        let start = first.len_utf8();
        let end = raw.len() - last.len_utf8();
        return Some(raw[start..end].to_string());
    }
    Some(raw.to_string())
}

fn normalize_route_path(route_path: &str, prefix: &str) -> String {
    let path_part = route_path.trim().trim_matches('/');
    let prefix_part = prefix.trim().trim_matches('/');
    let joined = if prefix_part.is_empty() {
        format!("/{path_part}")
    } else if path_part.is_empty() {
        format!("/{prefix_part}")
    } else {
        format!("/{prefix_part}/{path_part}")
    };
    collapse_slashes(&joined)
}

fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut previous_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !previous_slash {
                out.push(ch);
            }
            previous_slash = true;
        } else {
            out.push(ch);
            previous_slash = false;
        }
    }
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}

fn base_name_node(node: TsNode<'_>) -> Option<TsNode<'_>> {
    match node.kind() {
        "type_identifier" => Some(node),
        "scoped_type_identifier" => node.named_child(node.named_child_count().saturating_sub(1)),
        "generic_type" => node.named_child(0).and_then(base_name_node),
        _ => None,
    }
}

fn parse_import(node: TsNode<'_>, src: &str) -> Option<RawImport> {
    let raw_text = text(node, src);
    let mut body = raw_text.trim();
    body = body.strip_prefix("import")?.trim();
    let is_static = body.starts_with("static ");
    if is_static {
        body = body.strip_prefix("static")?.trim();
    }
    body = body.trim_end_matches(';').trim();
    if body.is_empty() {
        return None;
    }
    Some(RawImport {
        raw: body.to_string(),
        is_static,
        is_wildcard: body.ends_with(".*"),
        range: range_of(node),
    })
}

fn type_kind(node: TsNode<'_>) -> Option<NodeKind> {
    match node.kind() {
        "class_declaration" => Some(NodeKind::Class),
        "interface_declaration" => Some(NodeKind::Interface),
        "enum_declaration" => Some(NodeKind::Enum),
        "record_declaration" => Some(NodeKind::Record),
        "annotation_type_declaration" => Some(NodeKind::Annotation),
        _ => None,
    }
}

fn type_fqcn(package: Option<&str>, owner: Option<&TypeContext>, simple_name: &str) -> String {
    if let Some(owner) = owner {
        return format!("{}.{}", owner.fqcn, simple_name);
    }
    match package {
        Some(package) if !package.is_empty() => format!("{package}.{simple_name}"),
        _ => simple_name.to_string(),
    }
}

fn parameter_count(node: TsNode<'_>) -> u16 {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            count = count.saturating_add(1);
        }
    }
    count
}

fn param_type_names(node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        if let Some(ty) = child
            .child_by_field_name("type")
            .or_else(|| child.named_child(0))
        {
            out.push(text(ty, src));
        }
    }
    out
}

fn return_type_name(node: TsNode<'_>, src: &str) -> Option<String> {
    let ty = node.child_by_field_name("type")?;
    if ty.kind() == "void_type" {
        return None;
    }
    let raw = text(ty, src);
    (!raw.is_empty() && raw != "void").then_some(raw)
}

fn call_arity(node: TsNode<'_>) -> Option<u16> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut count = 0u16;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "block_comment" || child.kind() == "line_comment" {
            continue;
        }
        count = count.saturating_add(1);
    }
    Some(count)
}

/// Capture argument texts from a call node, truncated to 120 chars each (Gap 3).
fn capture_arg_texts(node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if matches!(child.kind(), "block_comment" | "line_comment") {
            continue;
        }
        let raw = text(child, src);
        if raw.is_empty() {
            continue;
        }
        let truncated = if raw.len() > 120 {
            format!("{}…", &raw[..120])
        } else {
            raw
        };
        out.push(truncated);
    }
    out
}

fn should_emit_field_read(node: TsNode<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    if parent.kind() != "assignment_expression" {
        return true;
    }
    parent
        .child_by_field_name("left")
        .is_none_or(|left| !same_node(left, node))
}

fn same_node(a: TsNode<'_>, b: TsNode<'_>) -> bool {
    a.kind() == b.kind() && a.start_byte() == b.start_byte() && a.end_byte() == b.end_byte()
}

fn context_for(byte: usize, builder: &FileBuilder) -> Option<String> {
    builder
        .callable_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
        .map(|ctx| ctx.in_fqcn.clone())
        .or_else(|| {
            type_context_at(byte, builder)
                .map(|ctx| ctx.fqcn.clone())
                .or_else(|| builder.package.clone())
        })
}

fn callable_context_at(byte: usize, builder: &FileBuilder) -> Option<&CallableContext> {
    builder
        .callable_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| ctx.start_byte)
}

fn callable_id_for(byte: usize, builder: &FileBuilder) -> NodeId {
    callable_context_at(byte, builder)
        .map(|ctx| ctx.id.clone())
        .or_else(|| type_context_at(byte, builder).map(|ctx| ctx.id.clone()))
        .unwrap_or_else(|| file_id(&builder.file))
}

fn type_context_at(byte: usize, builder: &FileBuilder) -> Option<&TypeContext> {
    builder
        .type_contexts
        .iter()
        .filter(|ctx| ctx.start_byte <= byte && byte <= ctx.end_byte)
        .max_by_key(|ctx| (ctx.start_byte, type_kind_rank(ctx.kind)))
}

fn type_kind_rank(kind: NodeKind) -> usize {
    match kind {
        NodeKind::Class => 5,
        NodeKind::Interface => 4,
        NodeKind::Enum => 3,
        NodeKind::Record => 2,
        NodeKind::Annotation => 1,
        _ => 0,
    }
}

fn modifiers(node: TsNode<'_>, src: &str) -> Vec<String> {
    let Some(modifier_node) = first_named_child(node, "modifiers") else {
        return Vec::new();
    };
    let raw = text(modifier_node, src);
    let known = [
        "public",
        "protected",
        "private",
        "abstract",
        "static",
        "final",
        "native",
        "synchronized",
        "transient",
        "volatile",
        "strictfp",
        "sealed",
        "non-sealed",
    ];
    let known_set = known.into_iter().collect::<BTreeSet<_>>();
    raw.split(|c: char| !c.is_ascii_alphabetic() && c != '-')
        .filter(|part| known_set.contains(part))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn class_stereotype(node: TsNode<'_>, src: &str, simple_name: &str) -> Option<&'static str> {
    for annotation in annotations(node) {
        let mapped = match annotation_name(annotation, src).as_deref() {
            Some("RestController") | Some("Controller") => "controller",
            Some("Service") => "service",
            Some("Repository") => "repository",
            Some("Configuration") => "configuration",
            Some("Component") => "component",
            Some("Entity") => "entity",
            Some("Path") => "resource",
            Some("SpringBootTest")
            | Some("ExtendWith")
            | Some("RunWith")
            | Some("WebMvcTest")
            | Some("DataJpaTest")
            | Some("DataMongoTest")
            | Some("JsonTest") => "test",
            _ => continue,
        };
        return Some(mapped);
    }
    if simple_name.ends_with("Test")
        || simple_name.ends_with("Tests")
        || simple_name.ends_with("IT")
        || simple_name.ends_with("Spec")
    {
        return Some("test");
    }
    // Name-suffix fallbacks — lower priority than annotations above.
    // Covers non-standard naming: Endpoint (Quarkus/WebFlux), Resource (JAX-RS),
    // Api (OpenAPI-generated), Handler (HTTP handlers), Facade/Service/Repository conventions.
    for (suffix, stereo) in [
        ("Controller", "controller"),
        ("Endpoint", "controller"),
        ("Resource", "resource"),
        ("Api", "controller"),
        ("Handler", "handler"),
        ("Facade", "service"),
        ("Repository", "repository"),
        ("Service", "service"),
    ] {
        if simple_name.ends_with(suffix) {
            return Some(stereo);
        }
    }
    None
}

fn is_bean_method(node: TsNode<'_>, src: &str) -> bool {
    annotations(node)
        .into_iter()
        .any(|ann| annotation_name(ann, src).as_deref() == Some("Bean"))
}

fn is_test_method(node: TsNode<'_>, src: &str) -> bool {
    annotations(node).into_iter().any(|ann| {
        matches!(
            annotation_name(ann, src).as_deref(),
            Some("Test") | Some("ParameterizedTest") | Some("RepeatedTest")
        )
    })
}

fn is_mock_or_injected_field(node: TsNode<'_>, src: &str) -> bool {
    annotations(node).into_iter().any(|ann| {
        matches!(
            annotation_name(ann, src).as_deref(),
            Some("MockBean")
                | Some("SpyBean")
                | Some("Autowired")
                | Some("InjectMocks")
                | Some("Mock")
        )
    })
}

fn simple_type_name(raw: &str) -> &str {
    let s = raw.trim();
    let s = s.split('<').next().unwrap_or(s);
    let s = s.split('[').next().unwrap_or(s);
    s.trim()
}

const JPA_INTERFACES: &[&str] = &[
    "JpaRepository",
    "CrudRepository",
    "PagingAndSortingRepository",
    "ListCrudRepository",
    "ListPagingAndSortingRepository",
    "MongoRepository",
    "ReactiveCrudRepository",
    "ReactiveMongoRepository",
    "R2dbcRepository",
    "JpaSpecificationExecutor",
];

fn jpa_repository_props(node: TsNode<'_>, src: &str) -> (bool, Option<String>) {
    let Some(interfaces_node) = node.child_by_field_name("interfaces") else {
        return (false, None);
    };
    let scan_node = first_named_child(interfaces_node, "interface_type_list")
        .or_else(|| first_named_child(interfaces_node, "type_list"))
        .unwrap_or(interfaces_node);
    let mut cursor = scan_node.walk();
    for child in scan_node.named_children(&mut cursor) {
        match child.kind() {
            "type_identifier" => {
                let name = text(child, src);
                if JPA_INTERFACES.contains(&name.as_str()) {
                    return (true, None);
                }
            }
            "generic_type" => {
                let Some(name_node) = child.named_child(0) else {
                    continue;
                };
                let name = text(name_node, src);
                if JPA_INTERFACES.contains(&name.as_str()) {
                    let entity = child
                        .named_child(1)
                        .and_then(|args| args.named_child(0))
                        .map(|c| text(c, src))
                        .filter(|s| !s.is_empty());
                    return (true, entity);
                }
            }
            _ => {}
        }
    }
    (false, None)
}

fn build_class_props(node: TsNode<'_>, src: &str, simple_name: &str) -> Option<serde_json::Value> {
    let stereotype = class_stereotype(node, src, simple_name);
    let (is_jpa, entity_opt) = jpa_repository_props(node, src);
    let effective_stereotype = stereotype.or(if is_jpa { Some("repository") } else { None });

    // For @Entity classes, extract @Table(name=...) so the DB-access emit pass can
    // produce DbTable nodes without needing a traceable SQL call chain.
    // We scan the raw source text of the class node header rather than walking the
    // annotation AST, because annotation_argument_list field names vary across
    // tree-sitter grammar versions.
    let table_name: Option<String> = if effective_stereotype == Some("entity") {
        let start = node.start_byte();
        // The class keyword can be a few hundred bytes after the opening annotation.
        // Grab up to 512 bytes from the node start to capture @Table before `class`.
        let header = &src[start..src.len().min(start + 512)];
        // Match @Table( ... name = "tablename" ... ) — name attribute first or anywhere
        extract_table_annotation_name(header)
    } else {
        None
    };

    let mut obj = serde_json::Map::new();
    if let Some(s) = effective_stereotype { obj.insert("stereotype".into(), s.into()); }
    if let Some(e) = entity_opt           { obj.insert("entityType".into(), e.into()); }
    if let Some(t) = table_name           { obj.insert("tableName".into(), t.into()); }
    if obj.is_empty() { None } else { Some(serde_json::Value::Object(obj)) }
}

/// Extract the `name` attribute value from an `@Table(name = "…")` annotation in raw
/// source text.  Handles multi-line annotations and quoted identifiers.
fn extract_table_annotation_name(text: &str) -> Option<String> {
    // Find @Table followed by '('
    let at_table = text.find("@Table")?;
    let after = &text[at_table + "@Table".len()..];
    let paren = after.find('(')?;
    let args = &after[paren + 1..];
    // Find 'name' key (not inside a nested annotation's name attribute — stop at class keyword)
    let class_pos = args.find("class ").unwrap_or(args.len());
    let search_area = &args[..class_pos.min(args.len())];
    // Look for:  name   =   "value"
    let mut pos = 0;
    while pos < search_area.len() {
        if let Some(rel) = search_area[pos..].find("name") {
            let abs = pos + rel;
            let after_name = search_area[abs + 4..].trim_start();
            if after_name.starts_with('=') {
                let after_eq = after_name[1..].trim_start();
                if after_eq.starts_with('"') {
                    let value_start = after_eq[1..].to_string();
                    let end = value_start.find('"')?;
                    let name = value_start[..end].to_string();
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            pos = abs + 4;
        } else {
            break;
        }
    }
    None
}

fn first_named_child<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    let result = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind);
    result
}

/// Build a 25-float StructuralProfile for every class/interface/enum node in the builder.
/// Must be called after all defs are collected but before nodes are sorted/deduped.
fn attach_structural_profiles(builder: &mut FileBuilder) {
    use cih_core::NodeKind;

    // Count extends/implements per class FQCN from reference_sites.
    let mut extends_count: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    let mut implements_count: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    for site in &builder.reference_sites {
        match site.kind {
            cih_core::RefKind::Extends => {
                *extends_count.entry(site.in_fqcn.as_str()).or_insert(0) += 1;
            }
            cih_core::RefKind::Implements => {
                *implements_count.entry(site.in_fqcn.as_str()).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    // Collect method-level complexity by owner FQCN.
    // For methods/constructors, def.fqcn == owner class FQCN.
    let mut method_cx: std::collections::HashMap<&str, Vec<&ComplexityRecord>> =
        std::collections::HashMap::new();
    let mut method_counts: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    let mut field_counts: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    let mut ctor_counts: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    for def in &builder.defs {
        match def.kind {
            NodeKind::Method => {
                *method_counts.entry(def.fqcn.as_str()).or_insert(0) += 1;
                if let Some(cx) = &def.complexity {
                    method_cx.entry(def.fqcn.as_str()).or_default().push(cx);
                }
            }
            NodeKind::Field => {
                *field_counts.entry(def.fqcn.as_str()).or_insert(0) += 1;
            }
            NodeKind::Constructor => {
                *ctor_counts.entry(def.fqcn.as_str()).or_insert(0) += 1;
                if let Some(cx) = &def.complexity {
                    method_cx.entry(def.fqcn.as_str()).or_default().push(cx);
                }
            }
            _ => {}
        }
    }

    // For each class-like def, compute the profile and attach to its Node.
    for def in &builder.defs {
        let is_class_like = matches!(
            def.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Annotation
        );
        if !is_class_like {
            continue;
        }

        let fqcn = def.fqcn.as_str();
        let cxs = method_cx.get(fqcn).map(Vec::as_slice).unwrap_or(&[]);
        let n = cxs.len() as f32;

        let avg_of = |f: fn(&ComplexityRecord) -> f32| -> f32 {
            if n == 0.0 { 0.0 } else { cxs.iter().map(|c| f(c)).sum::<f32>() / n }
        };
        let max_of = |f: fn(&ComplexityRecord) -> f32| -> f32 {
            cxs.iter().map(|c| f(c)).fold(0f32, f32::max)
        };
        let sum_of = |f: fn(&ComplexityRecord) -> f32| -> f32 {
            cxs.iter().map(|c| f(c)).sum::<f32>()
        };

        let loc = (def.range.end_line.saturating_sub(def.range.start_line)) as f32 / 1000.0;

        let features: [f32; 25] = [
            *method_counts.get(fqcn).unwrap_or(&0) as f32,           // 0 method_count
            *field_counts.get(fqcn).unwrap_or(&0) as f32,            // 1 field_count
            *ctor_counts.get(fqcn).unwrap_or(&0) as f32,             // 2 constructor_count
            avg_of(|c| c.cyclomatic as f32),                          // 3 avg_cyclomatic
            max_of(|c| c.cyclomatic as f32),                          // 4 max_cyclomatic
            avg_of(|c| c.cognitive as f32),                           // 5 avg_cognitive
            max_of(|c| c.cognitive as f32),                           // 6 max_cognitive
            avg_of(|c| c.loop_depth as f32),                          // 7 avg_loop_depth
            max_of(|c| c.loop_depth as f32),                          // 8 max_loop_depth
            sum_of(|c| c.if_count as f32),                            // 9 if_count
            sum_of(|c| c.for_count as f32),                           // 10 for_count
            sum_of(|c| c.while_count as f32),                         // 11 while_count
            sum_of(|c| c.switch_count as f32),                        // 12 switch_count
            sum_of(|c| c.try_count as f32),                           // 13 try_count
            sum_of(|c| c.return_count as f32),                        // 14 return_count
            sum_of(|c| c.throw_count as f32),                         // 15 throw_count
            def.framework_role.is_some() as u8 as f32,                    // 16 annotation_count (proxy)
            def.framework_role.is_some() as u8 as f32,                    // 17 has_framework_stereotype
            (def.kind == NodeKind::Interface) as u8 as f32,           // 18 is_interface
            def.modifiers.iter().any(|m| m == "abstract") as u8 as f32, // 19 is_abstract
            (def.kind == NodeKind::Enum) as u8 as f32,                // 20 is_enum
            *implements_count.get(fqcn).unwrap_or(&0) as f32,         // 21 implements_count
            *extends_count.get(fqcn).unwrap_or(&0) as f32,            // 22 extends_count
            (def.framework_role.as_deref() == Some("test")) as u8 as f32, // 23 is_test
            loc.min(1.0),                                              // 24 loc_normalized
        ];

        let profile = StructuralProfile { features };
        let sp_json = profile.to_json_array();

        // Find the matching node and attach the profile to its props.
        for node in &mut builder.nodes {
            if node.id == def.id {
                let props = node.props.get_or_insert_with(|| serde_json::json!({}));
                if let serde_json::Value::Object(ref mut map) = props {
                    map.insert("sp".to_string(), sp_json);
                }
                break;
            }
        }
    }
}

fn normalize_builder(builder: &mut FileBuilder) {
    builder
        .nodes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    builder.nodes.dedup_by(|a, b| a.id == b.id);
    builder.edges.sort_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then(a.dst.as_str().cmp(b.dst.as_str()))
            .then(a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    builder
        .edges
        .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
    builder.defs.sort_by(|a, b| {
        a.id.as_str()
            .cmp(b.id.as_str())
            .then(a.range.start_line.cmp(&b.range.start_line))
    });
    builder.defs.dedup_by(|a, b| a.id == b.id);
    builder.imports.sort_by(|a, b| {
        a.range
            .start_line
            .cmp(&b.range.start_line)
            .then(a.raw.cmp(&b.raw))
    });
    builder.imports.dedup_by(|a, b| {
        a.raw == b.raw
            && a.is_static == b.is_static
            && a.is_wildcard == b.is_wildcard
            && a.range == b.range
    });
    builder.reference_sites.sort_by(|a, b| {
        a.range
            .start_line
            .cmp(&b.range.start_line)
            .then(a.range.start_col.cmp(&b.range.start_col))
            .then(a.name.cmp(&b.name))
            .then(a.kind_key().cmp(b.kind_key()))
    });
    builder.reference_sites.dedup_by(|a, b| {
        a.name == b.name
            && a.receiver == b.receiver
            && a.kind == b.kind
            && a.arity == b.arity
            && a.range == b.range
            && a.in_fqcn == b.in_fqcn
    });
    builder.type_bindings.sort_by(|a, b| {
        a.in_fqcn
            .cmp(&b.in_fqcn)
            .then(a.name.cmp(&b.name))
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.start_col.cmp(&b.range.start_col))
            .then(a.raw_type.cmp(&b.raw_type))
    });
    builder.type_bindings.dedup_by(|a, b| {
        a.name == b.name
            && a.raw_type == b.raw_type
            && a.kind == b.kind
            && a.in_fqcn == b.in_fqcn
            && a.range == b.range
    });
    builder.contract_sites.sort_by(|a, b| {
        a.in_callable
            .as_str()
            .cmp(b.in_callable.as_str())
            .then(contract_kind_key(&a.kind).cmp(contract_kind_key(&b.kind)))
            .then(a.http_method.cmp(&b.http_method))
            .then(a.url_template.cmp(&b.url_template))
            .then(a.topic.cmp(&b.topic))
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.start_col.cmp(&b.range.start_col))
    });
    builder.contract_sites.dedup_by(|a, b| {
        a.kind == b.kind
            && a.url_template == b.url_template
            && a.topic == b.topic
            && a.http_method == b.http_method
            && a.in_callable == b.in_callable
            && a.range == b.range
    });
}

fn contract_kind_key(kind: &ContractKind) -> &str {
    match kind {
        ContractKind::HttpCall => "http-call",
        ContractKind::HttpClientProxy => "http-client-proxy",
        ContractKind::EventPublish => "event-publish",
        ContractKind::EventListen => "event-listen",
        ContractKind::Custom(s) => s.as_str(),
    }
}

trait RefKindKey {
    fn kind_key(&self) -> &'static str;
}

impl RefKindKey for ReferenceSite {
    fn kind_key(&self) -> &'static str {
        match self.kind {
            RefKind::Call => "call",
            RefKind::FieldRead => "field-read",
            RefKind::FieldWrite => "field-write",
            RefKind::Ctor => "ctor",
            RefKind::Extends => "extends",
            RefKind::Implements => "implements",
            RefKind::TypeRef => "type-ref",
        }
    }
}

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
