//! Generic AST / text helpers shared across the TypeScript parser modules
//! (the walker, the `Builder`, and each framework detector). Pure functions over
//! tree-sitter nodes + source text — no `Builder` state.

use cih_core::Range;
use tree_sitter::Node as TsNode;

pub(super) fn range_of(node: TsNode<'_>) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start_line: start.row as u32 + 1,
        start_col: start.column as u32,
        end_line: end.row as u32 + 1,
        end_col: end.column as u32,
    }
}

pub(super) fn text(node: TsNode<'_>, src: &str) -> String {
    node.utf8_text(src.as_bytes())
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(super) fn unquote(raw: &str) -> String {
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

pub(super) fn module_path(rel: &str) -> String {
    // TypeScript + JavaScript extensions (longest/most-specific first).
    for ext in [".tsx", ".jsx", ".mjs", ".cjs", ".ts", ".js"] {
        if let Some(stripped) = rel.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    rel.to_string()
}

pub(super) fn parameter_count(node: TsNode<'_>) -> u16 {
    let params = node.child_by_field_name("parameters");
    let Some(params) = params else {
        // A single-param arrow without parens (`x => x`) has no `parameters` list —
        // tree-sitter gives it a singular `parameter` field. Without this it reports
        // arity 0, which lands in the node id (`#name/0`) and breaks `find_member`'s
        // arity match.
        return node.child_by_field_name("parameter").map_or(0, |_| 1);
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

pub(super) fn first_string_arg_in_call(call_node: TsNode<'_>, src: &str) -> Option<String> {
    let args = call_node.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() == "string" {
            return Some(unquote(&text(child, src)));
        }
    }
    None
}

/// Value node of `{ key: value }` pair `key_name` in an `object` literal.
pub(super) fn object_pair_value<'a>(obj: TsNode<'a>, key_name: &str, src: &str) -> Option<TsNode<'a>> {
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

pub(super) fn call_arity(node: TsNode<'_>) -> Option<u16> {
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

pub(super) fn ts_positional_argument(call: TsNode<'_>, n: usize) -> Option<TsNode<'_>> {
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

/// Text of a plain string literal (`'…'` / `"…"`) — template strings and
/// expressions are not literals.
pub(super) fn literal_ts_string(node: TsNode<'_>, src: &str) -> Option<String> {
    (node.kind() == "string").then(|| unquote(&text(node, src)))
}
