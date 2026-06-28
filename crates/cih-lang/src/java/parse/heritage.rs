use cih_core::{file_id, NodeId, RefKind, ReferenceSite};
use tree_sitter::Node as TsNode;

use super::{FileBuilder, base_name_node, range_of, text, type_context_at};

pub(super) fn collect_heritage_references(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
) {
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
