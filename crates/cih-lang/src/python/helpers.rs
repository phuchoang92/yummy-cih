//! Generic AST / text helpers shared across the Python parser modules — pure
//! functions over tree-sitter nodes + source text, no `Builder` state.

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
    // Handle triple-quoted strings
    for delim in &["\"\"\"", "'''"] {
        if s.starts_with(delim) && s.ends_with(delim) && s.len() >= 6 {
            return s[3..s.len() - 3].to_string();
        }
    }
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'\'' || first == b'"') && first == last {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

pub(super) fn module_path(rel: &str) -> String {
    let stripped = rel.strip_suffix(".py").unwrap_or(rel);
    stripped.replace(['/', '\\'], ".")
}

/// Normalize a relative import (`.api_client`, `..pkg.mod`, `.`) against the
/// importing file's repo-relative path into a dotted absolute module. One
/// package level is stripped per leading dot beyond the first; walking above
/// the repo root returns `None`.
pub(super) fn normalize_relative_import(spec: &str, rel: &str) -> Option<String> {
    let dots = spec.chars().take_while(|c| *c == '.').count();
    if dots == 0 {
        return None;
    }
    let remainder = &spec[dots..];
    let mut package: Vec<&str> = rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("").split('/').filter(|part| !part.is_empty()).collect();
    for _ in 1..dots {
        package.pop()?;
    }
    for segment in remainder.split('.').filter(|segment| !segment.is_empty()) {
        package.push(segment);
    }
    if package.is_empty() {
        return None;
    }
    Some(package.join("."))
}

pub(super) fn parameter_count(node: TsNode<'_>) -> u16 {
    let Some(params) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "identifier"
            | "typed_parameter"
            | "typed_default_parameter"
            | "default_parameter"
            | "list_splat_pattern"
            | "dictionary_splat_pattern"
            | "keyword_separator"
            | "positional_separator" => {
                count = count.saturating_add(1);
            }
            _ => {}
        }
    }
    // Subtract self/cls if present
    count.saturating_sub(1)
}

pub(super) fn parse_attribute_or_identifier(node: TsNode<'_>, src: &str) -> (Option<String>, Option<String>) {
    if node.kind() == "attribute" {
        let obj = node.child_by_field_name("object").map(|n| text(n, src));
        let attr = node.child_by_field_name("attribute").map(|n| text(n, src));
        (obj, attr)
    } else {
        (None, Some(text(node, src)))
    }
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

pub(super) fn call_arity(node: TsNode<'_>) -> Option<u16> {
    let args = node.child_by_field_name("arguments")?;
    let mut count = 0u16;
    let mut cursor = args.walk();
    for _child in args.named_children(&mut cursor) {
        count = count.saturating_add(1);
    }
    Some(count)
}

/// Nth positional (non-keyword) argument of a call.
pub(super) fn positional_argument(call: TsNode<'_>, n: usize) -> Option<TsNode<'_>> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let mut index = 0;
    for child in args.named_children(&mut cursor) {
        if matches!(child.kind(), "keyword_argument" | "comment") {
            continue;
        }
        if index == n {
            return Some(child);
        }
        index += 1;
    }
    None
}

pub(super) fn literal_py_string(node: TsNode<'_>, src: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "interpolation")
    {
        return None;
    }
    let raw = text(node, src);
    let stripped = raw
        .strip_prefix(|c: char| matches!(c, 'f' | 'F' | 'r' | 'R' | 'b' | 'B' | 'u' | 'U'))
        .unwrap_or(&raw);
    Some(unquote(stripped))
}
