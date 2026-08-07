//! End-to-end gate for Spring AOP resolution: the full `analyze` pipeline over
//! `tests/corpus/java-spring-aop` must yield the exact expected `ADVISES` edge
//! set — annotation metadata retained by the parser, pointcuts parsed and
//! matched in `cih-resolve` post-processing, edges written to artifacts.

use std::path::{Path, PathBuf};

use cih_engine::analyze::analyze_emit;
use cih_engine::scan;
use cih_engine::scope::ScopeRequest;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join("java-spring-aop")
}

/// Analyze in a temp copy so the vendored tree keeps no `.cih/` and the run
/// can never be served by a cache (mirrors `corpus_coverage.rs`).
fn analyze_corpus() -> String {
    let src = corpus_dir();
    let dst = std::env::temp_dir().join(format!("cih-aop-corpus-{}", std::process::id()));
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
fn spring_aop_pointcuts_produce_exact_advises_edges() {
    let edges = analyze_corpus();
    let mut advises: Vec<(String, String)> = edges
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some("Advises"))
        .map(|e| {
            (
                e["src"].as_str().unwrap_or_default().to_string(),
                e["dst"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    advises.sort();

    let aspect = "Method:com.acme.aspect.LoggingAspect";
    let expected: Vec<(String, String)> = vec![
        // @AfterReturning via named @Pointcut → @annotation(Loggable): pay + create.
        (
            format!("{aspect}#auditLoggable/2"),
            "Method:com.acme.service.OrderService#pay/2".into(),
        ),
        (
            format!("{aspect}#auditLoggable/2"),
            "Method:com.acme.web.OrderController#create/1".into(),
        ),
        // @Before bean(orderService): both OrderService methods.
        (
            format!("{aspect}#beforeOrderServiceBean/1"),
            "Method:com.acme.service.OrderService#pay/2".into(),
        ),
        (
            format!("{aspect}#beforeOrderServiceBean/1"),
            "Method:com.acme.service.OrderService#refund/1".into(),
        ),
        // @Around execution over com.acme.service.*: both OrderService methods.
        // Helper.fmt is not a bean and must not appear anywhere.
        (
            format!("{aspect}#logServiceCalls/1"),
            "Method:com.acme.service.OrderService#pay/2".into(),
        ),
        (
            format!("{aspect}#logServiceCalls/1"),
            "Method:com.acme.service.OrderService#refund/1".into(),
        ),
    ];
    assert_eq!(
        advises, expected,
        "ADVISES edge set from the full analyze pipeline diverged"
    );
}
