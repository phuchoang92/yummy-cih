use cih_core::{SqlConstant, SqlExecutionSite, StringConstant};
use tree_sitter::Node as TsNode;

use super::{
    FileBuilder, callable_id_for, context_for, modifiers, range_of, receiver_has_type, text,
    type_context_at, unquote_spring_literal,
};

pub(super) fn collect_sql_constants(root: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    collect_sql_constants_in(root, src, builder, None);
}

pub(super) fn collect_static_string_constants(
    root: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
) {
    collect_static_string_constants_in(root, src, builder, None);
}

fn collect_static_string_constants_in(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner_fqcn: Option<&str>,
) {
    match node.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let fqcn = type_context_at(node.start_byte() + 1, builder).map(|t| t.fqcn.clone());
            let effective = fqcn.as_deref().or(owner_fqcn);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_static_string_constants_in(child, src, builder, effective);
            }
            return;
        }
        "field_declaration" => {
            if let Some(owner) = owner_fqcn {
                if let Some(sc) = try_extract_string_constant(node, src, owner) {
                    builder.string_constants.push(sc);
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_static_string_constants_in(child, src, builder, owner_fqcn);
    }
}

fn try_extract_string_constant(
    node: TsNode<'_>,
    src: &str,
    owner_fqcn: &str,
) -> Option<StringConstant> {
    let mods = modifiers(node, src);
    if !mods.iter().any(|m| m == "static") || !mods.iter().any(|m| m == "final") {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    if text(type_node, src) != "String" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let name_node = child.child_by_field_name("name")?;
        let const_name = text(name_node, src);
        if const_name.is_empty() {
            continue;
        }
        let Some(value_node) = child.child_by_field_name("value") else {
            continue;
        };
        let (value, dynamic) = fold_string_init(value_node, src);
        if value.is_empty() && !dynamic {
            continue;
        }
        return Some(StringConstant {
            const_name,
            owner_fqcn: owner_fqcn.to_string(),
            value,
            dynamic,
            range: range_of(node),
        });
    }
    None
}

fn collect_sql_constants_in(
    node: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
    owner_fqcn: Option<&str>,
) {
    match node.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let fqcn = type_context_at(node.start_byte() + 1, builder).map(|t| t.fqcn.clone());
            let effective = fqcn.as_deref().or(owner_fqcn);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_sql_constants_in(child, src, builder, effective);
            }
            return;
        }
        "field_declaration" => {
            if let Some(owner) = owner_fqcn {
                if let Some(constant) = try_extract_sql_constant(node, src, owner) {
                    builder.sql_constants.push(constant);
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_sql_constants_in(child, src, builder, owner_fqcn);
    }
}

fn try_extract_sql_constant(
    node: TsNode<'_>,
    src: &str,
    owner_fqcn: &str,
) -> Option<SqlConstant> {
    let mods = modifiers(node, src);
    if !mods.iter().any(|m| m == "static") || !mods.iter().any(|m| m == "final") {
        return None;
    }
    let type_node = node.child_by_field_name("type")?;
    if text(type_node, src) != "String" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let name_node = child.child_by_field_name("name")?;
        let const_name = text(name_node, src);
        if const_name.is_empty()
            || !const_name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            continue;
        }
        let Some(value_node) = child.child_by_field_name("value") else {
            continue;
        };
        let (sql_text, dynamic) = fold_string_init(value_node, src);
        if sql_text.is_empty() && !dynamic {
            continue;
        }
        return Some(SqlConstant {
            const_name,
            owner_fqcn: owner_fqcn.to_string(),
            sql_text,
            dynamic,
            range: range_of(node),
        });
    }
    None
}

fn fold_string_init(node: TsNode<'_>, src: &str) -> (String, bool) {
    match node.kind() {
        "string_literal" => {
            let raw = text(node, src);
            let inner = if raw.len() >= 2 { &raw[1..raw.len() - 1] } else { "" };
            let unescaped = inner
                .replace("\\n", " ")
                .replace("\\r", " ")
                .replace("\\t", " ")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            (unescaped, false)
        }
        "binary_expression" => {
            if node
                .child_by_field_name("operator")
                .map(|op| text(op, src))
                .as_deref()
                != Some("+")
            {
                return (String::new(), true);
            }
            let left = node
                .child_by_field_name("left")
                .map(|n| fold_string_init(n, src))
                .unwrap_or_default();
            let right = node
                .child_by_field_name("right")
                .map(|n| fold_string_init(n, src))
                .unwrap_or_default();
            (format!("{}{}", left.0, right.0), left.1 || right.1)
        }
        "parenthesized_expression" => {
            if let Some(inner) = node.named_child(0) {
                fold_string_init(inner, src)
            } else {
                (String::new(), true)
            }
        }
        _ => (String::new(), true),
    }
}

pub(super) fn collect_sql_execution_sites(
    root: TsNode<'_>,
    src: &str,
    builder: &mut FileBuilder,
) {
    collect_sql_execution_sites_in(root, src, builder);
}

fn collect_sql_execution_sites_in(node: TsNode<'_>, src: &str, builder: &mut FileBuilder) {
    if node.kind() == "method_invocation" {
        if let Some(site) = try_extract_execution_site(node, src, builder) {
            builder.sql_execution_sites.push(site);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_sql_execution_sites_in(child, src, builder);
    }
}

const DBUTIL_METHODS: &[&str] = &["prepareStatement", "executeQuery", "executeUpdate"];
const JDBC_TEMPLATE_METHODS: &[&str] = &[
    "query",
    "update",
    "queryForObject",
    "queryForList",
    "queryForMap",
    "batchUpdate",
];

fn try_extract_execution_site(
    node: TsNode<'_>,
    src: &str,
    builder: &FileBuilder,
) -> Option<SqlExecutionSite> {
    let method_name_node = node.child_by_field_name("name")?;
    let method = text(method_name_node, src);
    let range = range_of(node);
    let in_callable = callable_id_for(node.start_byte(), builder);
    let in_fqcn = context_for(node.start_byte(), builder).unwrap_or_default();

    let object = node.child_by_field_name("object")?;
    let receiver = text(object, src);

    if receiver == "DBUtil" && DBUTIL_METHODS.contains(&method.as_str()) {
        let const_ref = nth_identifier_argument(node, src, 1);
        if const_ref.is_some() || method == "prepareStatement" {
            return Some(SqlExecutionSite {
                api_name: method,
                const_ref,
                inline_sql: None,
                in_callable,
                range,
            });
        }
    }

    if JDBC_TEMPLATE_METHODS.contains(&method.as_str())
        && receiver_has_type(builder, &in_fqcn, &receiver, "JdbcTemplate")
    {
        let (const_ref, inline_sql) = first_sql_argument(node, src);
        if const_ref.is_some() || inline_sql.is_some() {
            return Some(SqlExecutionSite {
                api_name: method,
                const_ref,
                inline_sql,
                in_callable,
                range,
            });
        }
    }

    None
}

fn nth_identifier_argument(node: TsNode<'_>, src: &str, n: usize) -> Option<String> {
    let arguments = node.child_by_field_name("arguments")?;
    let mut count = 0;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            if count == n {
                let name = text(child, src);
                if !name.is_empty() {
                    return Some(name);
                }
            }
            count += 1;
        } else if !matches!(child.kind(), "line_comment" | "block_comment") && count <= n {
            count += 1;
        }
    }
    None
}

fn first_sql_argument(node: TsNode<'_>, src: &str) -> (Option<String>, Option<String>) {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return (None, None);
    };
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => return (Some(text(child, src)), None),
            "string_literal" => {
                return (None, unquote_spring_literal(&text(child, src)));
            }
            _ => continue,
        }
    }
    (None, None)
}
