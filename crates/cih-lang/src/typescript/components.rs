//! Component stereotype + dependency-injection detection — React component
//! stereotypes and NestJS/Angular constructor-injection type references.

use tree_sitter::Node as TsNode;


use super::builder::Builder;
use super::helpers::*;

// ── Component stereotypes + DI (P4) ───────────────────────────────────────────

/// True if the class extends `React.Component` / `Component` / `PureComponent`.
pub(super) fn class_extends_react_component(node: TsNode<'_>, src: &str) -> bool {
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if child.kind() == "class_heritage" {
            return text(child, src).contains("Component");
        }
    }
    false
}

/// Stereotype for a top-level function: React component (PascalCase) or hook
/// (`use<Upper>`), gated on a `react` import (the grammar can't confirm JSX).
pub(super) fn react_function_stereotype(name: &str, builder: &Builder) -> Option<String> {
    if !builder.imports_pkg("react") {
        return None;
    }
    let rest = name.strip_prefix("use");
    if matches!(rest.and_then(|r| r.chars().next()), Some(c) if c.is_uppercase()) {
        return Some("react_hook".to_string());
    }
    if name.chars().next().is_some_and(|c| c.is_uppercase()) {
        return Some("react_component".to_string());
    }
    None
}

/// A class stereotype that participates in constructor DI (Nest/Angular provider).
pub(super) fn is_di_provider(stereotype: Option<&str>) -> bool {
    matches!(
        stereotype,
        Some("nestjs_controller")
            | Some("nestjs_injectable")
            | Some("angular_injectable")
            | Some("angular_component")
            | Some("graphql_resolver")
    )
}

/// Simple type name from a heritage clause value: `A` → A, `React.Component` → B
/// (last property), `Base<T>` → Base. The resolver keys on this name.
pub(super) fn heritage_type_name(node: TsNode<'_>, src: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(text(node, src)),
        "member_expression" => node.child_by_field_name("property").map(|p| text(p, src)),
        "generic_type" => {
            let mut c = node.walk();
            let base = node
                .named_children(&mut c)
                .find(|n| matches!(n.kind(), "type_identifier" | "identifier" | "member_expression"))
                .and_then(|n| heritage_type_name(n, src));
            base
        }
        _ => None,
    }
}

/// Simple type name from a `type_annotation` node (`: User` → `User`,
/// `: Repository<User>` → `Repository`); `None` for primitives/unions/etc.
pub(super) fn type_annotation_name(annotation: TsNode<'_>, src: &str) -> Option<String> {
    let mut c = annotation.walk();
    let ty = annotation.named_children(&mut c).next()?;
    match ty.kind() {
        "type_identifier" => Some(text(ty, src)),
        "generic_type" => {
            let mut c2 = ty.walk();
            let base = ty
                .named_children(&mut c2)
                .find(|n| n.kind() == "type_identifier")
                .map(|n| text(n, src));
            base
        }
        _ => None,
    }
}

/// Simple type name of a constructor parameter's `: Type` annotation
/// (`private svc: UserService` → `UserService`; `Repository<User>` → `Repository`).
pub(super) fn param_type_name(param: TsNode<'_>, src: &str) -> Option<String> {
    let mut c = param.walk();
    let ann = param
        .named_children(&mut c)
        .find(|n| n.kind() == "type_annotation")?;
    let mut c2 = ann.walk();
    let ty = ann.named_children(&mut c2).next()?;
    match ty.kind() {
        "type_identifier" => Some(text(ty, src)),
        "generic_type" => {
            let mut c3 = ty.walk();
            let base = ty
                .named_children(&mut c3)
                .find(|n| n.kind() == "type_identifier")
                .map(|n| text(n, src));
            base
        }
        _ => None,
    }
}



