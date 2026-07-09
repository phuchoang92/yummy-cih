//! DB-access emit pass: `SqlConstant` + `SqlExecutionSite` IR → `DbQuery`/`DbTable`
//! nodes and `EXECUTES_QUERY`/`READS_TABLE`/`WRITES_TABLE` edges.
//!
//! Constant resolution uses a two-tier lookup:
//!   1. Same-file: `(file, const_name)` — highest confidence.
//!   2. Workspace-unique: `const_name` only, across all parsed files. If exactly one
//!      constant carries that name the site is resolved cross-file; if multiple exist
//!      the name is ambiguous and the site is emitted with `dynamic=true` and no table
//!      edges (same as the old v1 "truly unknown" behaviour).

use rustc_hash::FxHashSet;
use std::collections::{HashMap, HashSet};

use cih_core::{
    db_query_const_id, db_query_inline_id, db_table_id, Edge, EdgeKind, Node, NodeId, NodeKind,
    ParsedFile, Range,
};
use cih_parse::sql::{scan_tables, TableOp};

/// Emit `DbQuery` / `DbTable` nodes and DB edges from the SQL IR in `parsed`.
pub fn emit_db_access(parsed: &[ParsedFile]) -> (Vec<Node>, Vec<Edge>) {
    // Tier-1 index: (file, const_name) → SqlConstant — same-file lookup.
    let mut const_index: HashMap<(&str, &str), &cih_core::SqlConstant> = HashMap::new();
    // Tier-2 index: const_name → all constants with that name across all files.
    // Used when tier-1 misses: if exactly one entry exists the constant is workspace-unique
    // and can be resolved cross-file; if multiple exist the name is ambiguous → dynamic=true.
    let mut const_by_name: HashMap<&str, Vec<&cih_core::SqlConstant>> = HashMap::new();
    for pf in parsed {
        for c in &pf.sql_constants {
            const_index.insert((&pf.file, &c.const_name), c);
            const_by_name.entry(c.const_name.as_str()).or_default().push(c);
        }
    }

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_nodes: FxHashSet<NodeId> = FxHashSet::default();

    for pf in parsed {
        for site in &pf.sql_execution_sites {
            process_site(
                pf,
                site,
                &const_index,
                &const_by_name,
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
    const_by_name: &HashMap<&str, Vec<&cih_core::SqlConstant>>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    seen_nodes: &mut FxHashSet<NodeId>,
) {
    // Derive the owner FQCN from the callable id.
    // Method id format: `Method:<fqcn>#<name>/<arity>` or `Constructor:<fqcn>#<init>/<arity>`.
    let owner_fqcn = owner_fqcn_of(&site.in_callable);

    let (query_id, sql_text, dynamic, const_name_opt) = match (&site.const_ref, &site.inline_sql) {
        (Some(cref), _) => {
            // Tier 1: same-file lookup (highest confidence).
            let lookup_key = (&pf.file as &str, cref.as_str());
            if let Some(c) = const_index.get(&lookup_key) {
                let qid = db_query_const_id(&c.owner_fqcn, &c.const_name);
                (qid, c.sql_text.clone(), c.dynamic, Some(c.const_name.clone()))
            } else {
                // Tier 2: workspace-unique cross-file fallback.
                // If exactly one constant with this name exists across all parsed files,
                // resolve it regardless of which file defines it (covers the common pattern
                // of a shared SqlConstants utility class with statically-imported names).
                // If zero or multiple match, fall back to dynamic=true with no table edges.
                match const_by_name.get(cref.as_str()).map(Vec::as_slice) {
                    Some([c]) => {
                        let qid = db_query_const_id(&c.owner_fqcn, &c.const_name);
                        (qid, c.sql_text.clone(), c.dynamic, Some(c.const_name.clone()))
                    }
                    _ => {
                        let qid = db_query_const_id(owner_fqcn, cref);
                        (qid, String::new(), true, Some(cref.clone()))
                    }
                }
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
        props: None,
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
            props: None,
        });
    }
}

/// Derive owner FQCN from a method/constructor node id.
/// Format: `Method:<fqcn>#name/arity` or `Constructor:<fqcn>#<init>/n` or `Field:<fqcn>#name`.
#[doc(hidden)]
pub fn owner_fqcn_of(id: &NodeId) -> &str {
    let s = id.as_str();
    // Strip the kind prefix (up to first `:`).
    let after_colon = s.find(':').map(|i| &s[i + 1..]).unwrap_or(s);
    // The FQCN is everything before the `#`.
    after_colon
        .find('#')
        .map(|i| &after_colon[..i])
        .unwrap_or(after_colon)
}

/// Emit `DbTable` nodes (and linking edges) derived from JPA `@Entity` class nodes.
///
/// The Java parser records `props["tableName"]` on every `@Entity` class that also
/// has an `@Table(name=…)` annotation.  For entities without `@Table`, the JPA
/// default convention (`CamelCase → snake_case`, no prefix) is used — callers that
/// know the project's table prefix can refine this by inspecting the output.
///
/// Each entity gets a synthetic `DbQuery` node so that the existing WikiGraph
/// aggregation path (`member → executes_query → reads/writes_table`) picks it up
/// without any changes to `cih-wiki`.
pub fn emit_jpa_tables(nodes: &[Node]) -> (Vec<Node>, Vec<Edge>) {
    let mut out_nodes: Vec<Node> = Vec::new();
    let mut out_edges: Vec<Edge> = Vec::new();
    let mut seen_tables: HashSet<String> = HashSet::new();

    for node in nodes {
        if !matches!(
            node.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Record
        ) {
            continue;
        }
        let Some(props) = &node.props else { continue };
        if props.get("stereotype").and_then(|v| v.as_str()) != Some("entity") {
            continue;
        }

        let table_name: String = props
            .get("tableName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| camel_to_snake(&node.name));

        if table_name.is_empty() || seen_tables.contains(&table_name) {
            continue;
        }
        seen_tables.insert(table_name.clone());

        let table_id = db_table_id(&table_name);
        out_nodes.push(Node {
            id: table_id.clone(),
            kind: NodeKind::DbTable,
            name: table_name.clone(),
            qualified_name: None,
            file: String::new(),
            range: Range::default(),
            props: None,
        });

        // Synthetic DbQuery node so the wiki graph aggregation path works unchanged.
        let query_id = NodeId::new(format!("DbQuery:jpa:{}", node.id.as_str()));
        out_nodes.push(Node {
            id: query_id.clone(),
            kind: NodeKind::DbQuery,
            name: format!("jpa:{}", node.name),
            qualified_name: None,
            file: node.file.clone(),
            range: node.range,
            props: None,
        });

        // entity class → synthetic query
        out_edges.push(Edge {
            src: node.id.clone(),
            dst: query_id.clone(),
            kind: EdgeKind::ExecutesQuery,
            confidence: 1.0,
            reason: "jpa-entity".into(),
            props: None,
        });
        // synthetic query → table (both read and write; JPA entities are typically both)
        out_edges.push(Edge {
            src: query_id.clone(),
            dst: table_id.clone(),
            kind: EdgeKind::ReadsTable,
            confidence: 1.0,
            reason: "jpa-entity".into(),
            props: None,
        });
        out_edges.push(Edge {
            src: query_id,
            dst: table_id,
            kind: EdgeKind::WritesTable,
            confidence: 1.0,
            reason: "jpa-entity".into(),
            props: None,
        });
    }

    (out_nodes, out_edges)
}

fn camel_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
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
