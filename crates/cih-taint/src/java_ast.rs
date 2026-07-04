//! Low-level tree-sitter helpers shared by `java_ir` (statement IR) and `cfg` (CFG builder).
//!
//! All functions in this module are `pub(crate)` — they are implementation details
//! of the analysis pipeline and not part of the public API.

use tree_sitter::Node as TsNode;

use cih_core::{NodeId, Range};

// ── Text & range utilities ────────────────────────────────────────────────────

pub(crate) fn ts_text<'a>(node: TsNode<'a>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("").trim()
}

pub(crate) fn range_of(node: TsNode<'_>) -> Range {
    let s = node.start_position();
    let e = node.end_position();
    Range {
        start_line: s.row as u32 + 1,
        start_col: s.column as u32,
        end_line: e.row as u32 + 1,
        end_col: e.column as u32,
    }
}

pub(crate) fn stmt_id(callable_id: &NodeId, start_byte: usize) -> NodeId {
    NodeId::new(format!("{}:stmt:{start_byte}", callable_id.as_str()))
}

// ── Read/write collectors ─────────────────────────────────────────────────────

/// Recursively collect all `identifier` and `field_access` leaf texts into `out`.
/// Skips Java keywords and primitive type names.
pub(crate) fn collect_reads(node: TsNode<'_>, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            let t = ts_text(node, src);
            if !t.is_empty() && !is_noise_token(t) {
                out.push(t.to_string());
            }
        }
        "field_access" => {
            if let Some(field) = node.child_by_field_name("field") {
                let t = ts_text(field, src);
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_reads(child, src, out);
            }
        }
    }
}

pub(crate) fn is_noise_token(s: &str) -> bool {
    matches!(
        s,
        "true" | "false" | "null" | "this" | "super"
            | "int" | "long" | "double" | "float" | "boolean"
            | "char" | "byte" | "short" | "void" | "String"
            | "var" | "new"
    )
}

// ── Call-site helpers ─────────────────────────────────────────────────────────

/// Extract the unqualified callee name from a `method_invocation` node.
pub(crate) fn extract_call_site(node: TsNode<'_>, src: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .map(|n| ts_text(n, src).to_string())
        .filter(|s| !s.is_empty())
}

/// Collect identifier names from the argument list of a call node.
pub(crate) fn extract_call_args(node: TsNode<'_>, src: &[u8]) -> Vec<String> {
    let Some(args) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if matches!(child.kind(), "block_comment" | "line_comment") {
            continue;
        }
        collect_reads(child, src, &mut out);
    }
    out
}

/// Extract parameter names from a `formal_parameters` node.
pub(crate) fn extract_param_names(params: TsNode<'_>, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            if let Some(name_node) = child.child_by_field_name("name") {
                let n = ts_text(name_node, src).to_string();
                if !n.is_empty() {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Count formal parameters on a `method_declaration` / `constructor_declaration`.
pub(crate) fn count_params(node: TsNode<'_>) -> u16 {
    let Some(params) = node.child_by_field_name("parameters") else {
        return 0;
    };
    let mut count = 0u16;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            count = count.saturating_add(1);
        }
    }
    count
}

// ── Method locator ─────────────────────────────────────────────────────────────

/// Parse `method_name` and `arity` from a method node ID like
/// `Method:com.example.Class#save/2` or `Constructor:com.example.Class#<init>/1`.
pub(crate) fn parse_method_id(method_id: &NodeId) -> Option<(String, u16)> {
    let s = method_id.as_str();
    let s = s.split_once(':').map(|(_, r)| r).unwrap_or(s);
    let hash = s.rfind('#')?;
    let after_hash = &s[hash + 1..];
    let (name, arity_str) = if let Some(slash) = after_hash.rfind('/') {
        (&after_hash[..slash], &after_hash[slash + 1..])
    } else {
        (after_hash, "0")
    };
    let arity: u16 = arity_str.parse().unwrap_or(0);
    Some((name.to_string(), arity))
}

/// DFS over `root` looking for the first `method_declaration` or
/// `constructor_declaration` whose `name` field text equals `target_name`
/// and whose parameter count matches `target_arity`.
pub(crate) fn find_method_node<'tree>(
    root: TsNode<'tree>,
    src: &[u8],
    target_name: &str,
    target_arity: u16,
) -> Option<TsNode<'tree>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "method_declaration" | "constructor_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if ts_text(name_node, src) == target_name
                        && count_params(node) == target_arity
                    {
                        return Some(node);
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<TsNode<'tree>> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}
