//! lbug `Value` → domain conversions (the analog of `cih-falkor/src/serialize.rs`'s
//! read side), plus Cypher string-literal escaping for inlined filters.

use cih_core::{Node, NodeId, NodeKind, Range};
use lbug::Value;

/// Scalar cell → display string ("" for NULL). Mirrors the stringified-cell
/// model the Falkor adapter reads rows through, so row-parsing code ports 1:1.
pub(crate) fn cell_str(v: &Value) -> String {
    match v {
        Value::Null(_) => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Int32(i) => i.to_string(),
        Value::Int16(i) => i.to_string(),
        Value::Int8(i) => i.to_string(),
        Value::UInt64(i) => i.to_string(),
        Value::UInt32(i) => i.to_string(),
        Value::UInt16(i) => i.to_string(),
        Value::UInt8(i) => i.to_string(),
        Value::Int128(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Json(j) => j.to_string(),
        Value::List(_, items) | Value::Array(_, items) => {
            let inner: Vec<String> = items.iter().map(cell_str).collect();
            format!("[{}]", inner.join(", "))
        }
        other => format!("{other}"),
    }
}

pub(crate) fn cell_opt_str(v: &Value) -> Option<String> {
    let s = cell_str(v);
    (!s.is_empty()).then_some(s)
}

pub(crate) fn cell_u64(v: &Value) -> u64 {
    match v {
        Value::Int64(i) => (*i).max(0) as u64,
        Value::UInt64(i) => *i,
        Value::Int32(i) => (*i).max(0) as u64,
        _ => cell_str(v).parse().unwrap_or(0),
    }
}

pub(crate) fn cell_f64(v: &Value) -> f64 {
    match v {
        Value::Double(d) => *d,
        Value::Float(f) => *f as f64,
        Value::Int64(i) => *i as f64,
        _ => cell_str(v).parse().unwrap_or(0.0),
    }
}

/// Unpack a `RecursiveRel` cell: (hop count, interior node ids in pattern
/// order, rel labels in pattern order). Interior nodes exclude both endpoints.
pub(crate) fn recursive_rel(v: &Value) -> Option<(u32, Vec<String>, Vec<String>)> {
    let Value::RecursiveRel { nodes, rels } = v else {
        return None;
    };
    let interior = nodes
        .iter()
        .filter_map(|n| {
            n.get_properties()
                .iter()
                .find_map(|(k, val)| (k == "id").then(|| cell_str(val)).filter(|s| !s.is_empty()))
        })
        .collect();
    let labels = rels.iter().map(|r| r.get_label_name().clone()).collect();
    Some((rels.len() as u32, interior, labels))
}

/// Row of `(id, kind, name, qn, file)` string cells → `Node` (same column
/// convention as the Falkor adapter's `node_from_row`).
pub(crate) fn node_from_row(r: &[Value]) -> Node {
    Node {
        id: NodeId::new(r.first().map(cell_str).unwrap_or_default()),
        kind: NodeKind::from_label(&r.get(1).map(cell_str).unwrap_or_default()),
        name: r.get(2).map(cell_str).unwrap_or_default(),
        qualified_name: r.get(3).and_then(cell_opt_str),
        file: r.get(4).map(cell_str).unwrap_or_default(),
        range: Range::default(),
        props: None,
    }
}

/// Cypher string literal with escaping (`'...'`), for values inlined into
/// query text (same escaping as the Falkor adapter's `cstr`).
pub(crate) fn cstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// Optional Cypher string literal → `'...'` or `NULL`.
pub(crate) fn copt(s: Option<&str>) -> String {
    match s {
        Some(v) => cstr(v),
        None => "NULL".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cstr_escapes_quotes_and_newlines() {
        assert_eq!(cstr("a'b\nc"), "'a\\'b\\nc'");
    }

    #[test]
    fn cell_str_renders_scalars_and_null() {
        assert_eq!(cell_str(&Value::Int64(7)), "7");
        assert_eq!(cell_str(&Value::String("x".into())), "x");
        assert_eq!(cell_str(&Value::Null(lbug::LogicalType::String)), "");
    }
}
