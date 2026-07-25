//! End-to-end gate for qualifier-aware Spring XML DI resolution: the full
//! `analyze` pipeline over `tests/corpus/java-spring-xml-di` must redirect the
//! `@Qualifier("retailUserAdminRef")` interface-field call to the XML-wired
//! `UserImpl` — and must never emit the historical `CustomUserImpl → CustomUserImpl`
//! false self-recursion (bean id map collected before resolve, qualifier parsed
//! from the field, redirect skipping the enclosing class).

use std::path::{Path, PathBuf};

use cih_engine::analyze::analyze_emit;
use cih_engine::scan;
use cih_engine::scope::ScopeRequest;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join("java-spring-xml-di")
}

/// Analyze in a temp copy so the vendored tree keeps no `.cih/` and the run
/// can never be served by a cache (mirrors `aop_corpus.rs`). `tag` keeps the
/// copies of concurrently-running tests from clobbering each other.
fn analyze_corpus(tag: &str) -> String {
    let src = corpus_dir();
    let dst = std::env::temp_dir().join(format!("cih-di-corpus-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    copy_dir(&src, &dst).expect("copy corpus");

    let scan = scan::scan_repo(&dst).expect("scan corpus");
    let outcome = analyze_emit(
        &scan,
        ScopeRequest {
            all: true,
            ..ScopeRequest::default()
        },
    )
    .expect("analyze corpus");
    let edges = std::fs::read_to_string(&outcome.artifacts.edges_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dst);
    edges
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[test]
fn qualifier_field_call_redirects_to_xml_wired_impl() {
    let edges = analyze_corpus("qualifier");
    let calls: Vec<serde_json::Value> = edges
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some("Calls"))
        .collect();

    let caller = "Method:com.acme.user.CustomUserImpl#modifyUserPassword/1";
    let wired_impl = "Method:com.acme.user.UserImpl#modifyUserPassword/1";

    let redirect = calls
        .iter()
        .find(|e| e["src"] == caller && e["dst"] == wired_impl)
        .expect("qualifier call should redirect to the XML-wired UserImpl");
    assert_eq!(redirect["reason"], "di-qualifier");
    assert_eq!(redirect["confidence"].as_f64(), Some(0.95));

    assert!(
        !calls
            .iter()
            .any(|e| e["src"] == caller && e["dst"] == caller),
        "must not emit the false CustomUserImpl → CustomUserImpl self-recursion"
    );
}

/// The async audit chain: `UserImpl.modifyUserPassword` hands `INSERT_AUDIT_LOG`
/// to a custom queue API no allowlist names. The heuristic execution-site pass
/// must connect the method to the DbQuery and the query to `AUDIT_LOG` — the
/// exact table-write visibility the OCB trace investigation lacked.
#[test]
fn audit_queue_sql_constant_produces_table_write_edges() {
    let edges = analyze_corpus("audit");
    let all: Vec<serde_json::Value> = edges
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();

    let method = "Method:com.acme.user.UserImpl#modifyUserPassword/1";
    let executes = all
        .iter()
        .find(|e| e["kind"] == "ExecutesQuery" && e["src"] == method)
        .expect("heuristic EXECUTES_QUERY edge from the enqueueing method");
    let query_id = executes["dst"].as_str().expect("query id");
    assert!(
        query_id.contains("INSERT_AUDIT_LOG"),
        "query node is the named constant: {query_id}"
    );
    assert!(
        all.iter().any(|e| e["kind"] == "WritesTable"
            && e["src"] == query_id
            && e["dst"] == "DbTable:AUDIT_LOG"),
        "the audit INSERT must write AUDIT_LOG"
    );
}
