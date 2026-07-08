use std::path::{Path, PathBuf};

use cih_parse::{parse_files, LanguageRegistry};

fn java_registry() -> LanguageRegistry {
    let mut r = LanguageRegistry::new();
    r.register(cih_lang::java::JavaProvider::new());
    r
}

fn temp_repo() -> PathBuf {
    // pid + atomic counter: parallel tests in one binary share a pid and can
    // race to the same SystemTime nanos, so a timestamp alone collides.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "cih-parse-test-{}-{seq}-{nanos}",
        std::process::id()
    ))
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn parses_sql_constants_from_static_final_string_fields() {
    let root = temp_repo();
    let rel = "src/main/java/com/bank/OverdraftAdapterImpl.java";
    write_file(
        &root,
        rel,
        r#"
package com.bank;
public class OverdraftAdapterImpl {
private static final String QUERY_GET_BY_CODE =
    "SELECT id, amount FROM CUSTOM_OVERDRAFT_TYPE WHERE code = ?";
private static final String NOT_A_QUERY = "hello";
private String nonStatic = "SELECT FROM X";
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let parsed = output.parsed_files.first().unwrap();
    assert!(
        parsed.sql_constants.iter().any(|c| {
            c.const_name == "QUERY_GET_BY_CODE"
                && c.sql_text.contains("CUSTOM_OVERDRAFT_TYPE")
                && !c.dynamic
        }),
        "QUERY_GET_BY_CODE not extracted: {:?}",
        parsed.sql_constants
    );
    assert!(
        !parsed
            .sql_constants
            .iter()
            .any(|c| c.const_name == "nonStatic"),
        "non-static field must not be extracted"
    );
}

#[test]
fn parses_sql_constants_folds_string_concatenation() {
    let root = temp_repo();
    let rel = "src/main/java/com/bank/Adapter.java";
    write_file(
        &root,
        rel,
        r#"
package com.bank;
public class Adapter {
private static final String QUERY_CONCAT =
    "SELECT id FROM " +
    "CUSTOM_OVERDRAFT " +
    "WHERE id = ?";
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let parsed = output.parsed_files.first().unwrap();
    let c = parsed
        .sql_constants
        .iter()
        .find(|c| c.const_name == "QUERY_CONCAT")
        .expect("QUERY_CONCAT must be extracted");
    assert!(
        c.sql_text.contains("CUSTOM_OVERDRAFT"),
        "folded text: {:?}",
        c.sql_text
    );
    assert!(!c.dynamic, "pure literal concat must not be dynamic");
}

#[test]
fn parses_sql_constants_marks_dynamic_on_non_literal_concat() {
    let root = temp_repo();
    let rel = "src/main/java/com/bank/Adapter.java";
    write_file(
        &root,
        rel,
        r#"
package com.bank;
public class Adapter {
private static final String TABLE_NAME = "CUSTOM_OVERDRAFT";
private static final String QUERY_DYN = "SELECT id FROM " + TABLE_NAME + " WHERE id = ?";
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let parsed = output.parsed_files.first().unwrap();
    if let Some(c) = parsed
        .sql_constants
        .iter()
        .find(|c| c.const_name == "QUERY_DYN")
    {
        assert!(c.dynamic, "concat with identifier must be dynamic");
    }
}

#[test]
fn parses_sql_execution_sites_dbutil_pattern() {
    let root = temp_repo();
    let rel = "src/main/java/com/bank/OverdraftAdapterImpl.java";
    write_file(
        &root,
        rel,
        r#"
package com.bank;
import java.sql.Connection;
public class OverdraftAdapterImpl {
private static final String QUERY_GET = "SELECT id FROM CUSTOM_OVERDRAFT WHERE id = ?";

public Object getOverdraft(Connection conn, long id) {
    return DBUtil.executeQuery(conn, QUERY_GET, id);
}
}
"#,
    );
    let output = parse_files(&root, &[rel.to_string()], &java_registry()).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let parsed = output.parsed_files.first().unwrap();
    assert!(
        parsed.sql_execution_sites.iter().any(|s| {
            s.api_name == "executeQuery" && s.const_ref.as_deref() == Some("QUERY_GET")
        }),
        "DBUtil.executeQuery site not extracted: {:?}",
        parsed.sql_execution_sites
    );
}
