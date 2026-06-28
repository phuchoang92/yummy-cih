use std::collections::BTreeMap;

use cih_core::{BindingKind, RefKind, ReferenceSite, TypeBinding};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node as TsNode, QueryCursor, Tree};

use super::{
    FileBuilder, call_arity, callable_id_for, capture_arg_texts, context_for, parse_import,
    range_of, should_emit_field_read, text,
};
use crate::{java::JavaProvider, LanguageProvider};

pub(super) fn collect_query_ir(
    provider: &JavaProvider,
    tree: &Tree,
    src: &str,
    builder: &mut FileBuilder,
) {
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
