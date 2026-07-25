//! End-to-end gate for qualifier-aware Spring XML DI resolution: the full
//! `analyze` pipeline over `tests/corpus/java-spring-xml-di` must redirect the
//! constructor `@Qualifier("retailUserAdminRef")` propagated through an exact
//! `this.field = parameter` assignment, then used for an interface-field call to the XML-wired
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
struct CorpusArtifacts {
    nodes: String,
    edges: String,
}

fn analyze_corpus(tag: &str) -> CorpusArtifacts {
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
    let artifacts = CorpusArtifacts {
        nodes: std::fs::read_to_string(&outcome.artifacts.nodes_path).unwrap_or_default(),
        edges: std::fs::read_to_string(&outcome.artifacts.edges_path).unwrap_or_default(),
    };
    let _ = std::fs::remove_dir_all(&dst);
    artifacts
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
fn constructor_qualifier_field_call_redirects_to_xml_wired_impl() {
    let artifacts = analyze_corpus("qualifier");
    let calls: Vec<serde_json::Value> = artifacts
        .edges
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
    let artifacts = analyze_corpus("audit");
    let all: Vec<serde_json::Value> = artifacts
        .edges
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

#[test]
fn objectless_sql_constant_call_produces_query_edge() {
    let artifacts = analyze_corpus("objectless-sql");
    let all: Vec<serde_json::Value> = artifacts
        .edges
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect();

    assert!(
        all.iter().any(|edge| {
            edge["kind"] == "ExecutesQuery"
                && edge["src"] == "Method:com.acme.user.UserImpl#enqueueAudit/0"
                && edge["dst"] == "DbQuery:com.acme.user.UserImpl#INSERT_AUDIT_LOG"
        }),
        "objectless custom wrapper call must retain SQL-constant execution evidence"
    );
}

#[test]
fn route_handler_keeps_dotted_constructor_qualifier_redirect() {
    let artifacts = analyze_corpus("route");
    let nodes: Vec<serde_json::Value> = artifacts
        .nodes
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let edges: Vec<serde_json::Value> = artifacts
        .edges
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let route = "Route:POST /api/passwords/change";
    let handler = "Method:com.acme.user.PasswordController#change/1";

    assert!(nodes.iter().any(|node| node["id"] == route));
    assert!(edges.iter().any(|edge| edge["kind"] == "HandlesRoute"
        && edge["src"] == handler
        && edge["dst"] == route));
    let redirect = edges
        .iter()
        .find(|edge| {
            edge["kind"] == "Calls"
                && edge["src"] == handler
                && edge["dst"] == "Method:com.acme.user.UserImpl#modifyUserPassword/1"
        })
        .expect("this.userAdmin must retain its constructor-propagated qualifier");
    assert_eq!(redirect["reason"], "di-qualifier");
}

#[test]
fn async_spring_listener_emits_topic_and_listener_edge() {
    let artifacts = analyze_corpus("async-listener");
    let nodes: Vec<serde_json::Value> = artifacts
        .nodes
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let edges: Vec<serde_json::Value> = artifacts
        .edges
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let topic = "KafkaTopic:PasswordChangedEvent";
    let listener = "Method:com.acme.user.PasswordChangedListener#onPasswordChanged/1";

    assert!(nodes.iter().any(|node| node["id"] == topic));
    let edge = edges
        .iter()
        .find(|edge| edge["kind"] == "ListensTo" && edge["src"] == listener && edge["dst"] == topic)
        .expect("@Async @EventListener method should retain its Spring event-listen edge");
    assert_eq!(edge["props"]["messaging_framework"], "spring");
}

#[test]
fn unicode_non_sql_is_ignored_and_static_final_getter_is_not_accessor() {
    let artifacts = analyze_corpus("unicode-accessor");
    let nodes: Vec<serde_json::Value> = artifacts
        .nodes
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    assert!(
        !nodes.iter().any(|node| {
            node["kind"] == "DbQuery"
                && node["id"]
                    .as_str()
                    .is_some_and(|id| id.contains("unicodeLabel"))
        }),
        "non-SQL Unicode text must not create a query"
    );
    let getter = nodes
        .iter()
        .find(|node| node["id"] == "Method:com.acme.user.PasswordRequest#getDefault/0")
        .expect("constant getter method");
    assert_ne!(getter["props"]["isAccessor"], true);
    let field_getter = nodes
        .iter()
        .find(|node| node["id"] == "Method:com.acme.user.PasswordRequest#getNewPassword/0")
        .expect("field getter method");
    assert_eq!(field_getter["props"]["isAccessor"], true);
}

#[test]
fn malformed_adjacent_xml_keeps_valid_prefix_without_attribute_bleed() {
    let wiring = cih_resolve::di_xml::collect_di_wiring(&corpus_dir());

    assert_eq!(
        wiring
            .beans_by_id
            .get("validBeforeMalformedTail")
            .map(String::as_str),
        Some("com.acme.user.AuditQueue")
    );
    assert!(!wiring.beans_by_id.contains_key("missingClass"));
    assert!(!wiring
        .beans
        .iter()
        .any(|bean| bean.fqcn == "com.acme.user.NotABean" || bean.fqcn == "com.acme.user.Broken"));
}
