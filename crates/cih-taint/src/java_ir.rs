//! Java method body → in-memory statement IR via tree-sitter.
//!
//! [`extract_method_body`] re-parses the given source file, locates the requested
//! method by name+arity, and walks its body to produce a [`MethodBody`].
//! Callers should cache results by `(method_id, content_hash)` to avoid repeated
//! parses of large files.

use tree_sitter::{Node as TsNode, Parser};

use cih_core::{NodeId, Range};

use crate::ir::{MethodBody, StatementKind, StatementNode};

// ── Internal helpers (pub(crate) so cfg.rs can share them) ───────────────────

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
fn count_params(node: TsNode<'_>) -> u16 {
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
    // Drop "Method:" or "Constructor:" prefix.
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
        // Push children in reverse so we visit them left-to-right.
        let mut cursor = node.walk();
        let children: Vec<TsNode<'tree>> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}

// ── Statement walker ──────────────────────────────────────────────────────────

fn walk_block(block: TsNode<'_>, callable_id: &NodeId, src: &[u8], out: &mut Vec<StatementNode>) {
    let mut cursor = block.walk();
    let children: Vec<TsNode<'_>> = block.named_children(&mut cursor).collect();
    for child in children {
        classify_statement(child, callable_id, src, out);
    }
}

fn classify_statement(
    node: TsNode<'_>,
    callable_id: &NodeId,
    src: &[u8],
    out: &mut Vec<StatementNode>,
) {
    match node.kind() {
        // ── Variable declarations ─────────────────────────────────────────────
        "local_variable_declaration" => {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            let mut call_site = None;
            let mut call_args = Vec::new();

            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        writes.push(ts_text(name_node, src).to_string());
                    }
                    if let Some(value) = child.child_by_field_name("value") {
                        if matches!(value.kind(), "method_invocation" | "object_creation_expression") {
                            call_site = extract_call_site(value, src);
                            call_args = extract_call_args(value, src);
                        }
                        collect_reads(value, src, &mut reads);
                    }
                }
            }

            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Assign,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes,
                call_site,
                call_args,
            });
        }

        // ── Expression statements: assignment, call, increment, etc. ──────────
        "expression_statement" => {
            let mut cursor = node.walk();
            let inner = node.named_children(&mut cursor).next();
            if let Some(inner) = inner {
                emit_expression_statement(inner, node, callable_id, src, out);
            }
        }

        // ── Conditionals ──────────────────────────────────────────────────────
        "if_statement" => {
            let mut reads = Vec::new();
            if let Some(cond) = node.child_by_field_name("condition") {
                collect_reads(cond, src, &mut reads);
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Branch,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
            if let Some(then) = node.child_by_field_name("consequence") {
                if then.kind() == "block" {
                    walk_block(then, callable_id, src, out);
                } else {
                    classify_statement(then, callable_id, src, out);
                }
            }
            if let Some(alt) = node.child_by_field_name("alternative") {
                match alt.kind() {
                    "block" => walk_block(alt, callable_id, src, out),
                    _ => classify_statement(alt, callable_id, src, out),
                }
            }
        }

        "switch_statement" | "switch_expression" => {
            let mut reads = Vec::new();
            if let Some(cond) = node.child_by_field_name("condition") {
                collect_reads(cond, src, &mut reads);
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Branch,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                let groups: Vec<TsNode<'_>> = body.named_children(&mut cursor).collect();
                for group in groups {
                    let mut cursor2 = group.walk();
                    let stmts: Vec<TsNode<'_>> = group.named_children(&mut cursor2).collect();
                    for stmt in stmts {
                        if !matches!(stmt.kind(), "switch_label" | "switch_rule_expression") {
                            classify_statement(stmt, callable_id, src, out);
                        }
                    }
                }
            }
        }

        // ── Loops ─────────────────────────────────────────────────────────────
        "while_statement" | "do_statement" => {
            let mut reads = Vec::new();
            if let Some(cond) = node.child_by_field_name("condition") {
                collect_reads(cond, src, &mut reads);
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Loop,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
            if let Some(body) = node.child_by_field_name("body") {
                if body.kind() == "block" {
                    walk_block(body, callable_id, src, out);
                }
            }
        }

        "for_statement" => {
            let mut reads = Vec::new();
            // Collect reads from init / condition / update (all children except body).
            let mut cursor = node.walk();
            let children: Vec<TsNode<'_>> = node.named_children(&mut cursor).collect();
            for child in &children {
                if child.kind() != "block" {
                    collect_reads(*child, src, &mut reads);
                }
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Loop,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
            if let Some(body) = node.child_by_field_name("body") {
                if body.kind() == "block" {
                    walk_block(body, callable_id, src, out);
                }
            }
        }

        "enhanced_for_statement" => {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            if let Some(val) = node.child_by_field_name("value") {
                collect_reads(val, src, &mut reads);
            }
            if let Some(name) = node.child_by_field_name("name") {
                writes.push(ts_text(name, src).to_string());
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Loop,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes,
                call_site: None,
                call_args: Vec::new(),
            });
            if let Some(body) = node.child_by_field_name("body") {
                if body.kind() == "block" {
                    walk_block(body, callable_id, src, out);
                }
            }
        }

        // ── Return ────────────────────────────────────────────────────────────
        "return_statement" => {
            let mut reads = Vec::new();
            let mut call_site = None;
            let mut call_args = Vec::new();
            let mut cursor = node.walk();
            let children: Vec<TsNode<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                if matches!(child.kind(), "method_invocation" | "object_creation_expression") {
                    call_site = extract_call_site(child, src);
                    call_args = extract_call_args(child, src);
                }
                collect_reads(child, src, &mut reads);
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Return,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes: Vec::new(),
                call_site,
                call_args,
            });
        }

        // ── Throw ─────────────────────────────────────────────────────────────
        "throw_statement" => {
            let mut reads = Vec::new();
            let mut cursor = node.walk();
            let children: Vec<TsNode<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                collect_reads(child, src, &mut reads);
            }
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Throw,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads,
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
        }

        // ── Try ───────────────────────────────────────────────────────────────
        "try_statement" | "try_with_resources_statement" => {
            out.push(StatementNode {
                id: stmt_id(callable_id, node.start_byte()),
                kind: StatementKind::Try,
                in_callable: callable_id.clone(),
                range: range_of(node),
                reads: Vec::new(),
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
            let mut cursor = node.walk();
            let children: Vec<TsNode<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                match child.kind() {
                    "block" => walk_block(child, callable_id, src, out),
                    "catch_clause" => {
                        if let Some(body) = child.child_by_field_name("body") {
                            walk_block(body, callable_id, src, out);
                        }
                    }
                    "finally_clause" => {
                        let mut cursor2 = child.walk();
                        for fc in child.named_children(&mut cursor2) {
                            if fc.kind() == "block" {
                                walk_block(fc, callable_id, src, out);
                            }
                        }
                    }
                    "resource_specification" => {} // handled inline by try-with-resources
                    _ => {}
                }
            }
        }

        // ── Nested block (synchronized, static initializer, etc.) ─────────────
        "block" | "synchronized_statement" => {
            walk_block(node, callable_id, src, out);
        }

        // ── Ignore: comments, labels, assertions, break/continue ──────────────
        _ => {}
    }
}

/// Emit a `StatementNode` for an expression that is the direct child of an
/// `expression_statement` node (`node` is the expression; `stmt_node` is the
/// wrapping statement used for the location/id).
fn emit_expression_statement(
    inner: TsNode<'_>,
    stmt_node: TsNode<'_>,
    callable_id: &NodeId,
    src: &[u8],
    out: &mut Vec<StatementNode>,
) {
    match inner.kind() {
        "assignment_expression" | "compound_assignment_expression" => {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            let mut call_site = None;
            let mut call_args = Vec::new();

            if let Some(left) = inner.child_by_field_name("left") {
                match left.kind() {
                    "identifier" => {
                        writes.push(ts_text(left, src).to_string());
                    }
                    "field_access" | "array_access" => {
                        if let Some(f) = left.child_by_field_name("field") {
                            writes.push(ts_text(f, src).to_string());
                        }
                    }
                    _ => {}
                }
            }
            if let Some(right) = inner.child_by_field_name("right") {
                if matches!(right.kind(), "method_invocation" | "object_creation_expression") {
                    call_site = extract_call_site(right, src);
                    call_args = extract_call_args(right, src);
                }
                collect_reads(right, src, &mut reads);
            }

            out.push(StatementNode {
                id: stmt_id(callable_id, stmt_node.start_byte()),
                kind: StatementKind::Assign,
                in_callable: callable_id.clone(),
                range: range_of(stmt_node),
                reads,
                writes,
                call_site,
                call_args,
            });
        }

        "method_invocation" | "object_creation_expression" => {
            let mut reads = Vec::new();
            let call_site = extract_call_site(inner, src);
            let call_args = extract_call_args(inner, src);
            collect_reads(inner, src, &mut reads);
            out.push(StatementNode {
                id: stmt_id(callable_id, stmt_node.start_byte()),
                kind: StatementKind::Call,
                in_callable: callable_id.clone(),
                range: range_of(stmt_node),
                reads,
                writes: Vec::new(),
                call_site,
                call_args,
            });
        }

        _ => {
            let mut reads = Vec::new();
            collect_reads(inner, src, &mut reads);
            out.push(StatementNode {
                id: stmt_id(callable_id, stmt_node.start_byte()),
                kind: StatementKind::Other,
                in_callable: callable_id.clone(),
                range: range_of(stmt_node),
                reads,
                writes: Vec::new(),
                call_site: None,
                call_args: Vec::new(),
            });
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse `src` as a Java source file and extract an in-memory [`MethodBody`] for
/// the method identified by `method_id`.
///
/// Returns `None` if:
/// - `method_id` cannot be parsed (not a valid method node ID format)
/// - tree-sitter fails to parse `src`
/// - no method with matching name/arity is found in the AST
pub fn extract_method_body(method_id: &NodeId, src: &str) -> Option<MethodBody> {
    let (target_name, target_arity) = parse_method_id(method_id)?;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(src.as_bytes(), None)?;
    let root = tree.root_node();

    let method_node =
        find_method_node(root, src.as_bytes(), &target_name, target_arity)?;

    let param_names = method_node
        .child_by_field_name("parameters")
        .map(|p| extract_param_names(p, src.as_bytes()))
        .unwrap_or_default();

    let body = method_node.child_by_field_name("body")?;

    let mut statements = Vec::new();
    walk_block(body, method_id, src.as_bytes(), &mut statements);

    Some(MethodBody {
        callable_id: method_id.clone(),
        param_names,
        statements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(s: &str) -> NodeId {
        NodeId::new(s)
    }

    #[test]
    fn parse_simple_method() {
        let src = r#"
class Foo {
    public String process(String input) {
        String result = sanitize(input);
        return result;
    }
}
"#;
        let id = mid("Method:com.example.Foo#process/1");
        let body = extract_method_body(&id, src).expect("should find method");
        assert_eq!(body.param_names, vec!["input"]);
        assert!(
            body.statements.len() >= 2,
            "expected at least 2 stmts, got {}",
            body.statements.len()
        );
        let assign = &body.statements[0];
        assert_eq!(assign.kind, StatementKind::Assign);
        assert!(assign.writes.contains(&"result".to_string()));
        assert!(assign.reads.contains(&"input".to_string()));
        let ret = &body.statements[1];
        assert_eq!(ret.kind, StatementKind::Return);
        assert!(ret.reads.contains(&"result".to_string()));
    }

    #[test]
    fn parse_method_not_found_returns_none() {
        let src = r#"class Foo { void bar() {} }"#;
        let id = mid("Method:com.example.Foo#nonexistent/0");
        assert!(extract_method_body(&id, src).is_none());
    }

    #[test]
    fn parse_if_and_call() {
        let src = r#"
class OrderService {
    void save(String query) {
        if (query != null) {
            jdbcTemplate.execute(query);
        }
    }
}
"#;
        let id = mid("Method:com.example.OrderService#save/1");
        let body = extract_method_body(&id, src).expect("should find method");
        assert_eq!(body.param_names, vec!["query"]);

        let branch = body
            .statements
            .iter()
            .find(|s| s.kind == StatementKind::Branch)
            .expect("expected a Branch statement");
        assert!(branch.reads.contains(&"query".to_string()));

        let call = body
            .statements
            .iter()
            .find(|s| s.kind == StatementKind::Call)
            .expect("expected a Call statement");
        assert_eq!(call.call_site.as_deref(), Some("execute"));
        assert!(call.call_args.contains(&"query".to_string()));
    }
}
