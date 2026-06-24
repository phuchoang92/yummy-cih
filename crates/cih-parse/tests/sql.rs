use cih_parse::sql::{scan_tables, TableOp};

fn reads(sql: &str) -> Vec<String> {
    scan_tables(sql)
        .into_iter()
        .filter(|t| t.op == TableOp::Read)
        .map(|t| t.table)
        .collect()
}

fn writes(sql: &str) -> Vec<String> {
    scan_tables(sql)
        .into_iter()
        .filter(|t| t.op == TableOp::Write)
        .map(|t| t.table)
        .collect()
}

#[test]
fn simple_select_from() {
    let r = reads("SELECT id, name FROM CUSTOM_OVERDRAFT WHERE id = ?");
    assert!(r.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {r:?}");
}

#[test]
fn select_from_with_join() {
    let r = reads("SELECT * FROM TABLE_A a JOIN TABLE_B b ON a.id = b.a_id");
    assert!(r.contains(&"TABLE_A".to_string()));
    assert!(r.contains(&"TABLE_B".to_string()));
}

#[test]
fn comma_join_style() {
    let r = reads("SELECT * FROM TABLE_A a, TABLE_B b WHERE a.id = b.id");
    assert!(r.contains(&"TABLE_A".to_string()), "got: {r:?}");
    assert!(r.contains(&"TABLE_B".to_string()), "got: {r:?}");
}

#[test]
fn insert_into_is_write() {
    let w = writes("INSERT INTO CUSTOM_OVERDRAFT (col1, col2) VALUES (?, ?)");
    assert!(w.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {w:?}");
    let r = reads("INSERT INTO CUSTOM_OVERDRAFT (col1, col2) VALUES (?, ?)");
    assert!(
        !r.contains(&"CUSTOM_OVERDRAFT".to_string()),
        "should not be a read"
    );
}

#[test]
fn update_is_write() {
    let w = writes("UPDATE CUSTOM_OVERDRAFT SET status = ? WHERE id = ?");
    assert!(w.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {w:?}");
}

#[test]
fn delete_from_is_write() {
    let w = writes("DELETE FROM CUSTOM_OVERDRAFT WHERE id = ?");
    assert!(w.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {w:?}");
}

#[test]
fn merge_into_writes_target_reads_source() {
    let sql = "MERGE INTO TARGET_TABLE t USING SOURCE_TABLE s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.val = s.val";
    let w = writes(sql);
    let r = reads(sql);
    assert!(w.contains(&"TARGET_TABLE".to_string()), "got writes: {w:?}");
    assert!(r.contains(&"SOURCE_TABLE".to_string()), "got reads: {r:?}");
    assert!(
        !r.contains(&"TARGET_TABLE".to_string()),
        "target must not be a read"
    );
}

#[test]
fn oracle_block_comment_hint_stripped() {
    let r = reads("SELECT /*+ INDEX(t IDX_OD_ID) */ * FROM CUSTOM_OVERDRAFT t WHERE t.id = ?");
    assert!(r.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {r:?}");
}

#[test]
fn line_comment_stripped() {
    let r = reads("SELECT * -- get all rows\nFROM CUSTOM_OVERDRAFT WHERE id = ?");
    assert!(r.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {r:?}");
}

#[test]
fn schema_qualified_name_stripped() {
    let r = reads("SELECT * FROM BANKING_SCHEMA.CUSTOM_OVERDRAFT WHERE id = ?");
    assert!(r.contains(&"CUSTOM_OVERDRAFT".to_string()), "got: {r:?}");
}

#[test]
fn dual_is_skipped() {
    let r = reads("SELECT SYSDATE FROM DUAL");
    assert!(r.is_empty(), "DUAL must not appear as a table: {r:?}");
}

#[test]
fn subquery_extracts_inner_table() {
    let r = reads("SELECT * FROM (SELECT id FROM TABLE_B WHERE active = 1) t");
    assert!(r.contains(&"TABLE_B".to_string()), "got: {r:?}");
}

#[test]
fn dynamic_sql_with_no_real_table_returns_empty() {
    assert!(scan_tables("? + ? WHERE").is_empty());
}

#[test]
fn deduplicates_same_table_same_op() {
    let r = reads("SELECT * FROM A JOIN A ON A.id = A.id");
    assert_eq!(r.iter().filter(|t| t.as_str() == "A").count(), 1);
}

#[test]
fn left_join_reads_table() {
    let r = reads("SELECT * FROM TABLE_A a LEFT JOIN TABLE_B b ON a.id = b.a_id");
    assert!(r.contains(&"TABLE_A".to_string()));
    assert!(r.contains(&"TABLE_B".to_string()));
}
