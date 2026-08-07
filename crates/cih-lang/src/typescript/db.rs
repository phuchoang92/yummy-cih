//! Database / ORM access detection — Prisma / Mongoose / Sequelize / Knex / TypeORM
//! queries and model definitions become `DbTable` nodes + read/write edges.

use cih_core::{
    file_id, NodeId,
};
use tree_sitter::Node as TsNode;


use super::builder::Builder;
use super::helpers::*;

// ── DB / ORM access (Prisma / Mongoose / Sequelize / Knex / TypeORM) ──────────

/// Classify an ORM method name as a DB op: `Some(is_write)`, or `None` if the
/// method is not a recognized data-access operation.
pub(super) fn db_op_kind(op: &str) -> Option<bool> {
    match op {
        "find" | "findOne" | "findById" | "findByPk" | "findAll" | "findMany"
        | "findUnique" | "findFirst" | "findUniqueOrThrow" | "findFirstOrThrow" | "count"
        | "aggregate" | "groupBy" | "exists" | "distinct"
        // Knex query-builder terminals (read).
        | "select" | "first" | "pluck" => Some(false),
        "create" | "createMany" | "save" | "insert" | "insertMany" | "bulkCreate" | "update"
        | "updateOne" | "updateMany" | "upsert" | "delete" | "deleteOne" | "deleteMany"
        | "destroy" | "remove" | "findOneAndUpdate" | "findOneAndDelete"
        | "findByIdAndUpdate" | "findByIdAndDelete"
        // Knex write terminals.
        | "del" | "increment" | "decrement" => Some(true),
        _ => None,
    }
}

/// A model-defining call → `(table_name, engine)`: `mongoose.model('T',…)`,
/// bare `model('T',…)` (mongoose named import), or `sequelize.define('T',…)`.
pub(super) fn db_model_definition(value: TsNode<'_>, src: &str) -> Option<(String, &'static str)> {
    if value.kind() != "call_expression" {
        return None;
    }
    let func = value.child_by_field_name("function")?;
    let engine = match func.kind() {
        "identifier" if text(func, src) == "model" => "mongoose",
        "member_expression" => {
            let obj = func
                .child_by_field_name("object")
                .map(|n| text(n, src))
                .unwrap_or_default();
            let prop = func
                .child_by_field_name("property")
                .map(|n| text(n, src))
                .unwrap_or_default();
            if prop == "define" {
                "sequelize"
            } else if obj == "mongoose" && prop == "model" {
                "mongoose"
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let table = first_string_arg_in_call(value, src)?;
    Some((table, engine))
}

/// Pre-pass: record ORM model vars (`const User = mongoose.model('User',…)`) →
/// table name, and emit the `DbTable` node.
pub(super) fn collect_db_models(root: TsNode<'_>, src: &str, builder: &mut Builder) {
    let rel = builder.rel.clone();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "variable_declarator" {
            if let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) {
                if name.kind() == "identifier" {
                    if let Some((table, _engine)) = db_model_definition(value, src) {
                        builder.db_models.insert(text(name, src), table.clone());
                        builder.emit_db_table(&table, &rel, range_of(n));
                    }
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
    }
}

/// Receiver base name for a Prisma call `<base>.<model>.<op>()`: `prisma` /
/// `this.prisma`.
pub(super) fn prisma_base_name(base: TsNode<'_>, src: &str) -> Option<String> {
    match base.kind() {
        "identifier" => Some(text(base, src)),
        "member_expression" => {
            let inner = base.child_by_field_name("object")?;
            if text(inner, src) == "this" {
                base.child_by_field_name("property").map(|p| text(p, src))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Unwind a Knex receiver chain (`knex('t').where(…).…`) to the root
/// `knex('t')` call and return the table literal.
pub(super) fn knex_root_table(mut recv: TsNode<'_>, src: &str, builder: &Builder) -> Option<String> {
    loop {
        match recv.kind() {
            "call_expression" => {
                let f = recv.child_by_field_name("function")?;
                if f.kind() == "identifier" {
                    let name = text(f, src);
                    if name == "knex" || (name == "db" && builder.imports_pkg("knex")) {
                        return first_string_arg_in_call(recv, src);
                    }
                    return None;
                }
                // Member call (`.where(…)`) — descend into its receiver.
                recv = f.child_by_field_name("object")?;
            }
            "member_expression" => recv = recv.child_by_field_name("object")?,
            _ => return None,
        }
    }
}

/// Detect an ORM data-access call and emit `DbQuery`/`DbTable` + edges: Prisma
/// (`prisma.model.op`), Mongoose/Sequelize model methods, and Knex query builders.
pub(super) fn try_emit_db_query(
    node: TsNode<'_>,
    src: &str,
    builder: &mut Builder,
    enclosing_fn: Option<&NodeId>,
) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "member_expression" {
        return;
    }
    let Some(prop_node) = func.child_by_field_name("property") else {
        return;
    };
    let op = text(prop_node, src);
    let Some(is_write) = db_op_kind(&op) else {
        return;
    };
    let Some(object) = func.child_by_field_name("object") else {
        return;
    };
    let in_callable = enclosing_fn
        .cloned()
        .unwrap_or_else(|| file_id(&builder.rel));

    // Prisma: `prisma.<model>.<op>()` — object is the `prisma.<model>` member.
    if object.kind() == "member_expression" {
        if let (Some(base), Some(model)) = (
            object.child_by_field_name("object"),
            object.child_by_field_name("property"),
        ) {
            if let Some(bn) = prisma_base_name(base, src) {
                let gated = bn == "prisma"
                    || (builder.imports_pkg("@prisma/client") && matches!(bn.as_str(), "db"));
                if gated {
                    let table = text(model, src);
                    builder.emit_db_query(node, &table, &op, "prisma", is_write, &in_callable);
                    return;
                }
            }
        }
    }

    // Mongoose/Sequelize model var: `User.find()`.
    if object.kind() == "identifier" {
        if let Some(table) = builder.db_models.get(&text(object, src)).cloned() {
            builder.emit_db_query(node, &table, &op, "orm", is_write, &in_callable);
            return;
        }
    }

    // Knex query builder: `knex('t').where(…).select()`.
    if let Some(table) = knex_root_table(object, src, builder) {
        builder.emit_db_query(node, &table, &op, "knex", is_write, &in_callable);
    }
}

