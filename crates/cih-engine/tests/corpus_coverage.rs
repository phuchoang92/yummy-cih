//! Extraction-coverage gate over a **real** vendored repo.
//!
//! # Why this test exists
//!
//! A CommonJS resolver fix once passed every unit test in this workspace and then
//! produced *zero* improvement on a real Express app — 10 resolved edges before, 10
//! after. The fix was correct. The tests were self-confirming: hand-written fixtures
//! only ever contain the idioms their author already had in mind, so the parser and
//! its tests shared the same blind spot (module-scope arrow consts, barrel
//! re-exports, directory imports — none of which any fixture used).
//!
//! Two properties make this test catch what unit tests structurally cannot:
//!
//! 1. **The corpus is real code we did not write** (`tests/corpus/js-cjs-express`,
//!    an MIT snapshot — see its `PROVENANCE.md`). It contains idioms nobody here
//!    enumerated.
//! 2. **It measures absence, not change.** `parse_schema_guard` hashes parser output,
//!    so an idiom the parser silently ignores hashes stably and passes forever. This
//!    asserts a *floor* on how much of the AST we actually extracted, so dropping an
//!    idiom fails loudly.
//!
//! # Reading a failure
//!
//! - **Coverage below floor** → the parser stopped recognizing a callable idiom.
//!   Diff `callable_node_count`; find which shape vanished.
//! - **Resolved edges below floor** → a resolution path regressed (bindings, module
//!   normalization, barrel chasing).
//! - **Numbers went UP** → good. Raise the floors to lock the win in.
//!
//! Floors sit just under measured values, so ordinary drift doesn't cause noise but
//! a real regression does.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cih_engine::analyze::{self, analyze_emit};
use cih_engine::scan;
use cih_engine::scope::ScopeRequest;

static TEST_ID: AtomicU64 = AtomicU64::new(0);

/// Measured on the vendored corpus. Coverage never reaches 1.0: anonymous inline
/// callbacks (`arr.map(x => x * 2)`) are callables that rightly never become nodes.
/// Before the arrow-const/barrel work these were 0.034 and 9 — the numbers that
/// should have raised the alarm on day one.
const MIN_CALLABLE_COVERAGE: f64 = 0.55; // measured 0.607
const MIN_RESOLVED_EDGES: usize = 50; // measured 59
const MIN_CALLABLE_NODES: usize = 48; // measured 54

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join("js-cjs-express")
}

/// Analyze the corpus in a temp copy so the vendored tree keeps no `.cih/` and the
/// run can never be served by a cache (a reused no-op reports zeros).
fn analyze_corpus() -> (analyze::EmitOutcome, String) {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let src = corpus_dir();
    let dst = std::env::temp_dir().join(format!("cih-corpus-{}-{id}", std::process::id()));
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
    // Read the edges before the temp tree goes away.
    let edges = std::fs::read_to_string(&outcome.artifacts.edges_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dst);
    (outcome, edges)
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
fn real_commonjs_repo_meets_extraction_floors() {
    let (out, _) = analyze_corpus();
    let coverage = analyze::callable_coverage(out.callable_node_count, out.syntactic_callables)
        .expect("typescript provider must measure callables");

    assert!(
        out.callable_node_count >= MIN_CALLABLE_NODES,
        "callable nodes {} < floor {} — the parser stopped recognizing a function \
         idiom that real code uses ({} callables in the AST)",
        out.callable_node_count,
        MIN_CALLABLE_NODES,
        out.syntactic_callables
    );
    assert!(
        coverage >= MIN_CALLABLE_COVERAGE,
        "callable coverage {coverage:.3} < floor {MIN_CALLABLE_COVERAGE} — \
         {} of {} callables became nodes. An idiom is being dropped silently.",
        out.callable_node_count,
        out.syntactic_callables
    );
    assert!(
        out.resolved_edge_count >= MIN_RESOLVED_EDGES,
        "resolved edges {} < floor {} — a resolution path regressed \
         (require bindings, module normalization, or barrel chasing)",
        out.resolved_edge_count,
        MIN_RESOLVED_EDGES
    );
}

/// The specific chain the CommonJS work exists to serve, pinned end-to-end on real
/// code: a controller reaching a service *through a barrel*. Asserting the shape
/// (not just a count) means this fails for a comprehensible reason.
#[test]
fn controller_reaches_service_through_barrel_in_real_repo() {
    let (_out, edges) = analyze_corpus();
    let crosses = edges.lines().any(|line| {
        line.contains(r#""kind":"Calls""#)
            && line.contains(r#""src":"Function:src/controllers/"#)
            && line.contains(r#""dst":"Function:src/services/"#)
    });
    assert!(
        crosses,
        "no controller→service CALLS edge. The chain is: \
         `const {{ userService }} = require('../services')` → ModuleMember binding → \
         `src/services` normalized to `src/services/index` → barrel ModuleRef → \
         `src/services/user.service` → the arrow-const Function node. \
         Every link must hold; this is exactly the path that used to yield 0 edges."
    );
}
