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
mod tests {
    use super::*;
    use cih_core::{method_id, NodeKind, ParsedFile, Range, SqlConstant, SqlExecutionSite};

    fn make_parsed_file(
        file: &str,
        _owner_fqcn: &str,
        sql_constants: Vec<SqlConstant>,
        sql_execution_sites: Vec<SqlExecutionSite>,
    ) -> ParsedFile {
        ParsedFile {
            file: file.to_string(),
            language: String::new(),
            package: None,
            defs: vec![],
            imports: vec![],
            reference_sites: vec![],
            type_bindings: vec![],
            contract_sites: vec![],
            sql_constants,
            sql_execution_sites,
        }
    }

    fn make_constant(const_name: &str, owner_fqcn: &str, sql: &str) -> SqlConstant {
        SqlConstant {
            const_name: const_name.to_string(),
            owner_fqcn: owner_fqcn.to_string(),
            sql_text: sql.to_string(),
            dynamic: false,
            range: Range::default(),
        }
    }

    fn make_site(api_name: &str, const_ref: Option<&str>, in_callable: NodeId) -> SqlExecutionSite {
        SqlExecutionSite {
            api_name: api_name.to_string(),
            const_ref: const_ref.map(str::to_string),
            inline_sql: None,
            in_callable,
            range: Range::default(),
        }
    }

    #[test]
    fn emit_db_access_emits_query_table_nodes_and_edges() {
        let fqcn = "com.bank.OverdraftAdapterImpl";
        let callable = method_id(fqcn, "getOverdraft", 1);
        let pf = make_parsed_file(
            "src/main/java/OverdraftAdapterImpl.java",
            fqcn,
            vec![make_constant(
                "QUERY_FOO",
                fqcn,
                "SELECT id, amount FROM CUSTOM_OVERDRAFT WHERE id = ?",
            )],
            vec![make_site(
                "executeQuery",
                Some("QUERY_FOO"),
                callable.clone(),
            )],
        );

        let (nodes, edges) = emit_db_access(&[pf]);

        let query_id = db_query_const_id(fqcn, "QUERY_FOO");
        let table_id = db_table_id("CUSTOM_OVERDRAFT");

        assert!(
            nodes
                .iter()
                .any(|n| n.id == query_id && n.kind == NodeKind::DbQuery),
            "DbQuery node missing"
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.id == table_id && n.kind == NodeKind::DbTable),
            "DbTable node missing"
        );
        assert!(
            edges.iter().any(|e| e.src == callable
                && e.dst == query_id
                && e.kind == EdgeKind::ExecutesQuery),
            "EXECUTES_QUERY edge missing"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.src == query_id && e.dst == table_id && e.kind == EdgeKind::ReadsTable),
            "READS_TABLE edge missing"
        );
    }

    #[test]
    fn emit_db_access_writes_table_uses_writes_table_edge() {
        let fqcn = "com.bank.OverdraftAdapterImpl";
        let callable = method_id(fqcn, "insertOverdraft", 1);
        let pf = make_parsed_file(
            "src/main/java/OverdraftAdapterImpl.java",
            fqcn,
            vec![make_constant(
                "QUERY_INSERT",
                fqcn,
                "INSERT INTO CUSTOM_OVERDRAFT (col1, col2) VALUES (?, ?)",
            )],
            vec![make_site(
                "executeUpdate",
                Some("QUERY_INSERT"),
                callable.clone(),
            )],
        );

        let (nodes, edges) = emit_db_access(&[pf]);

        let query_id = db_query_const_id(fqcn, "QUERY_INSERT");
        let table_id = db_table_id("CUSTOM_OVERDRAFT");

        assert!(
            edges
                .iter()
                .any(|e| e.src == query_id && e.dst == table_id && e.kind == EdgeKind::WritesTable),
            "WRITES_TABLE edge missing"
        );
        assert!(
            !edges
                .iter()
                .any(|e| e.src == query_id && e.dst == table_id && e.kind == EdgeKind::ReadsTable),
            "should not be READS_TABLE"
        );
        let _ = nodes;
    }

    #[test]
    fn emit_db_access_deduplicates_db_table_nodes() {
        let fqcn = "com.bank.OverdraftAdapterImpl";
        let callable1 = method_id(fqcn, "getByCode", 1);
        let callable2 = method_id(fqcn, "getByName", 1);
        let pf = make_parsed_file(
            "src/main/java/OverdraftAdapterImpl.java",
            fqcn,
            vec![
                make_constant(
                    "QUERY_BY_CODE",
                    fqcn,
                    "SELECT * FROM CUSTOM_OVERDRAFT WHERE code = ?",
                ),
                make_constant(
                    "QUERY_BY_NAME",
                    fqcn,
                    "SELECT * FROM CUSTOM_OVERDRAFT WHERE name = ?",
                ),
            ],
            vec![
                make_site("executeQuery", Some("QUERY_BY_CODE"), callable1),
                make_site("executeQuery", Some("QUERY_BY_NAME"), callable2),
            ],
        );

        let (nodes, _edges) = emit_db_access(&[pf]);

        let table_count = nodes
            .iter()
            .filter(|n| n.id == db_table_id("CUSTOM_OVERDRAFT"))
            .count();
        assert_eq!(
            table_count, 1,
            "DbTable node must be deduplicated: found {table_count}"
        );
    }

    #[test]
    fn emit_db_access_skips_site_with_unknown_const_ref() {
        let fqcn = "com.bank.AdapterImpl";
        let callable = method_id(fqcn, "doSomething", 0);
        // No SqlConstants defined in this file.
        let pf = make_parsed_file(
            "src/main/java/AdapterImpl.java",
            fqcn,
            vec![], // no constants
            vec![make_site(
                "executeQuery",
                Some("QUERY_FROM_OTHER_CLASS"),
                callable,
            )],
        );

        let (nodes, edges) = emit_db_access(&[pf]);

        // Should emit a DbQuery (with dynamic=true) but no DbTable nodes or table edges.
        let table_nodes: Vec<_> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::DbTable)
            .collect();
        assert!(
            table_nodes.is_empty(),
            "no DbTable should be emitted: {table_nodes:?}"
        );
        let table_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == EdgeKind::ReadsTable || e.kind == EdgeKind::WritesTable)
            .collect();
        assert!(
            table_edges.is_empty(),
            "no table edges should be emitted: {table_edges:?}"
        );
    }

    #[test]
    fn emit_db_access_marks_dynamic_in_props() {
        let fqcn = "com.bank.AdapterImpl";
        let callable = method_id(fqcn, "dynamicQuery", 1);
        let mut c = make_constant("QUERY_DYNAMIC", fqcn, "SELECT * FROM TABLE_A WHERE");
        c.dynamic = true;
        let pf = make_parsed_file(
            "src/main/java/AdapterImpl.java",
            fqcn,
            vec![c],
            vec![make_site("executeQuery", Some("QUERY_DYNAMIC"), callable)],
        );

        let (nodes, _) = emit_db_access(&[pf]);

        let query_id = db_query_const_id(fqcn, "QUERY_DYNAMIC");
        let qnode = nodes
            .iter()
            .find(|n| n.id == query_id)
            .expect("DbQuery node missing");
        let dynamic = qnode
            .props
            .as_ref()
            .and_then(|p| p.get("dynamic"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(dynamic, "dynamic prop must be true in DbQuery node");
    }

    #[test]
    fn owner_fqcn_of_extracts_fqcn_from_method_id() {
        let id = method_id("com.bank.Adapter", "doWork", 2);
        assert_eq!(owner_fqcn_of(&id), "com.bank.Adapter");
    }
}
