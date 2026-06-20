//! DB-access emit pass: `SqlConstant` + `SqlExecutionSite` IR → `DbQuery`/`DbTable`
//! nodes and `EXECUTES_QUERY`/`READS_TABLE`/`WRITES_TABLE` edges.
//!
//! Same-class constant resolution only (v1): if `const_ref` names a constant
//! defined in a different class the site is emitted with `dynamic=true` props and
//! no table edges.

use std::collections::{HashMap, HashSet};

use cih_core::{
    db_query_const_id, db_query_inline_id, db_table_id, Edge, EdgeKind, Node, NodeId, NodeKind,
    ParsedFile, Range,
};
use cih_parse::sql::{scan_tables, TableOp};

/// Emit `DbQuery` / `DbTable` nodes and DB edges from the SQL IR in `parsed`.
pub fn emit_db_access(parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>) {
    // Build a per-(file, owner_fqcn, const_name) → SqlConstant index.
    // We key by (file, const_name) since owner_fqcn can be derived from in_callable.
    let mut const_index: HashMap<(&str, &str), &cih_core::SqlConstant> = HashMap::new();
    for pf in parsed {
        for c in &pf.sql_constants {
            const_index.insert((&pf.file, &c.const_name), c);
        }
    }

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_nodes: HashSet<NodeId> = HashSet::new();

    for pf in parsed {
        for site in &pf.sql_execution_sites {
            process_site(
                pf,
                site,
                &const_index,
                &mut nodes,
                &mut edges,
                &mut seen_nodes,
            );
        }
    }

    (nodes, edges)
}

fn process_site(
    pf: &ParsedFile,
    site: &cih_core::SqlExecutionSite,
    const_index: &HashMap<(&str, &str), &cih_core::SqlConstant>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    seen_nodes: &mut HashSet<NodeId>,
) {
    // Derive the owner FQCN from the callable id.
    // Method id format: `Method:<fqcn>#<name>/<arity>` or `Constructor:<fqcn>#<init>/<arity>`.
    let owner_fqcn = owner_fqcn_of(&site.in_callable);

    let (query_id, sql_text, dynamic, const_name_opt) = match (&site.const_ref, &site.inline_sql) {
        (Some(cref), _) => {
            // Try same-file constant lookup first, then same-class via owner_fqcn.
            let lookup_key = (&pf.file as &str, cref.as_str());
            if let Some(c) = const_index.get(&lookup_key) {
                let qid = db_query_const_id(&c.owner_fqcn, &c.const_name);
                (
                    qid,
                    c.sql_text.clone(),
                    c.dynamic,
                    Some(c.const_name.clone()),
                )
            } else {
                // Cross-file or unknown constant: emit with dynamic=true, no tables.
                let qid = db_query_const_id(owner_fqcn, cref);
                (qid, String::new(), true, Some(cref.clone()))
            }
        }
        (None, Some(inline)) => {
            let qid = db_query_inline_id(&pf.file, site.range.start_line, site.range.start_col);
            (qid, inline.clone(), false, None)
        }
        (None, None) => return,
    };

    let table_accesses = if sql_text.is_empty() {
        Vec::new()
    } else {
        scan_tables(&sql_text)
    };

    // Determine primary SQL operation from the first write or read found.
    let primary_op = table_accesses
        .iter()
        .find(|t| t.op == TableOp::Write)
        .or_else(|| table_accesses.first())
        .map(|t| {
            if t.op == TableOp::Write {
                "WRITE"
            } else {
                "READ"
            }
        })
        .unwrap_or("UNKNOWN");

    // Derive dialect-level operation hint from SQL text keywords.
    let sql_op = detect_sql_op(&sql_text);
    let sql_preview = sql_text.chars().take(120).collect::<String>();
    let table_names: Vec<String> = table_accesses.iter().map(|t| t.table.clone()).collect();

    let props = serde_json::json!({
        "operation": sql_op,
        "primaryAccess": primary_op,
        "sqlPreview": sql_preview,
        "dynamic": dynamic,
        "tables": table_names,
        "dialect": "oracle-like",
    });
    let props = if let Some(name) = &const_name_opt {
        let mut obj = props.as_object().cloned().unwrap_or_default();
        obj.insert(
            "constantName".to_string(),
            serde_json::Value::String(name.clone()),
        );
        serde_json::Value::Object(obj)
    } else {
        props
    };

    // Emit DbQuery node (deduplicated).
    if !seen_nodes.contains(&query_id) {
        seen_nodes.insert(query_id.clone());
        let name = const_name_opt
            .as_deref()
            .unwrap_or("inline-sql")
            .to_string();
        nodes.push(Node {
            id: query_id.clone(),
            kind: NodeKind::DbQuery,
            name,
            qualified_name: None,
            file: pf.file.clone(),
            range: site.range,
            props: Some(props),
        });
    }

    // Emit EXECUTES_QUERY edge: in_callable → DbQuery.
    edges.push(Edge {
        src: site.in_callable.clone(),
        dst: query_id.clone(),
        kind: EdgeKind::ExecutesQuery,
        confidence: 1.0,
        reason: site.api_name.clone(),
    });

    // Emit DbTable nodes + READS_TABLE / WRITES_TABLE edges.
    for access in &table_accesses {
        let table_id = db_table_id(&access.table);
        if !seen_nodes.contains(&table_id) {
            seen_nodes.insert(table_id.clone());
            nodes.push(Node {
                id: table_id.clone(),
                kind: NodeKind::DbTable,
                name: access.table.clone(),
                qualified_name: None,
                file: String::new(),
                range: Range::default(),
                props: None,
            });
        }
        let edge_kind = if access.op == TableOp::Write {
            EdgeKind::WritesTable
        } else {
            EdgeKind::ReadsTable
        };
        edges.push(Edge {
            src: query_id.clone(),
            dst: table_id,
            kind: edge_kind,
            confidence: 1.0,
            reason: "sql-scan".into(),
        });
    }
}

/// Derive owner FQCN from a method/constructor node id.
/// Format: `Method:<fqcn>#name/arity` or `Constructor:<fqcn>#<init>/n` or `Field:<fqcn>#name`.
fn owner_fqcn_of(id: &NodeId) -> &str {
    let s = id.as_str();
    // Strip the kind prefix (up to first `:`).
    let after_colon = s.find(':').map(|i| &s[i + 1..]).unwrap_or(s);
    // The FQCN is everything before the `#`.
    after_colon
        .find('#')
        .map(|i| &after_colon[..i])
        .unwrap_or(after_colon)
}

/// Detect the top-level SQL operation keyword.
fn detect_sql_op(sql: &str) -> &'static str {
    let upper = sql.trim_start().to_ascii_uppercase();
    if upper.starts_with("SELECT") {
        "SELECT"
    } else if upper.starts_with("INSERT") {
        "INSERT"
    } else if upper.starts_with("UPDATE") {
        "UPDATE"
    } else if upper.starts_with("DELETE") {
        "DELETE"
    } else if upper.starts_with("MERGE") {
        "MERGE"
    } else if upper.starts_with("BEGIN") {
        "BEGIN"
    } else {
        "UNKNOWN"
    }
}

#[cfg(test)]
mod tests;

