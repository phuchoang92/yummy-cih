use cih_core::{
    constructor_id, field_id, file_id, method_id, type_id, Edge, EdgeKind, Node, NodeId,
    NodeKind, SymbolDef,
};
use tree_sitter::Node as TsNode;

use super::{
    CallableContext, FileBuilder, TypeContext, jaxrs_class_prefix, modifiers, param_type_names,
    parameter_count, range_of, return_type_name, spring_class_prefix, text, type_fqcn, type_kind,
};
use super::metrics::{compute_complexity, java_body_fingerprint};
use super::structural::{
    build_class_props, class_stereotype, is_bean_method, is_mock_or_injected_field, is_test_method,
    simple_type_name,
};

pub(super) fn collect_declarations(
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
            name: simple_name.clone(),
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

    let complexity = node
        .child_by_field_name("body")
        .map(|body| compute_complexity(body));

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
        if let Some(ref cx) = complexity {
            obj.insert("cyclomatic".into(), serde_json::Value::Number(cx.cyclomatic.into()));
            obj.insert("cognitive".into(), serde_json::Value::Number(cx.cognitive.into()));
            obj.insert("loopDepth".into(), serde_json::Value::Number(cx.loop_depth.into()));
        }
        if let Some(ref fp) = body_fingerprint {
            if let Ok(v) = serde_json::to_value(fp) {
                obj.insert("bodyFingerprint".into(), v);
            }
        }
        if obj.is_empty() { None } else { Some(serde_json::Value::Object(obj)) }
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

    let complexity = node
        .child_by_field_name("body")
        .map(|body| compute_complexity(body));

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
