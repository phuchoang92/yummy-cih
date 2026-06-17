use std::collections::HashSet;

use cih_core::{BindingKind, NodeKind, TypeBinding};

/// Remove duplicates from `v` without changing the order of the first occurrences.
pub(crate) fn stable_dedup(v: &mut Vec<String>) {
    let mut seen = HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}
pub(crate) fn is_type_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Enum
            | NodeKind::Record
            | NodeKind::Annotation
    )
}

/// Simple (last) segment of a dotted FQCN.
pub(crate) fn simple_of(fqcn: &str) -> String {
    fqcn.rsplit('.').next().unwrap_or(fqcn).to_string()
}

/// Enclosing class FQCN of a callable signature (`fqcn#name/arity` → `fqcn`).
pub(crate) fn class_of(in_fqcn: &str) -> &str {
    in_fqcn.split('#').next().unwrap_or(in_fqcn)
}

/// Strip generics and array brackets to the base type name.
pub(crate) fn base_type_name(raw: &str) -> String {
    raw.split('<')
        .next()
        .unwrap_or(raw)
        .replace("[]", "")
        .trim()
        .to_string()
}

/// Choose the best binding for `name`: by kind precedence, then latest range
/// (nearest declaration wins for shadowing).
pub(crate) fn pick_binding<'a>(bindings: &'a [TypeBinding], name: &str) -> Option<&'a TypeBinding> {
    bindings.iter().filter(|b| b.name == name).max_by(|a, b| {
        binding_rank(a.kind)
            .cmp(&binding_rank(b.kind))
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.start_col.cmp(&b.range.start_col))
    })
}

/// Higher rank wins. Params/locals beat patterns; aliases/call-results last.
fn binding_rank(kind: BindingKind) -> u8 {
    match kind {
        BindingKind::Param => 6,
        BindingKind::Local => 5,
        BindingKind::Pattern => 4,
        BindingKind::Field => 3,
        BindingKind::CallResult => 2,
        BindingKind::Alias => 1,
        BindingKind::Return => 0,
    }
}

pub(crate) fn is_simple_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn starts_uppercase(value: &str) -> bool {
    value
        .chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
}

pub(crate) fn call_name(expr: &str) -> Option<&str> {
    let open = expr.rfind('(')?;
    let name = expr[..open].trim();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn split_last_dot_outside_parens(value: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (idx, ch) in value.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => {
                let left = value[..idx].trim();
                let right = value[idx + 1..].trim();
                if !left.is_empty() && !right.is_empty() {
                    return Some((left, right));
                }
            }
            _ => {}
        }
    }
    None
}
