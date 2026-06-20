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
