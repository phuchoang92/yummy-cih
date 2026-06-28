use cih_core::{ContractKind, RefKind, ReferenceSite};

use super::FileBuilder;

pub(super) fn normalize_builder(builder: &mut FileBuilder) {
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

