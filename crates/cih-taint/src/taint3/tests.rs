use super::*;
use crate::cfg::build_cfg;
use crate::pdg::{build_pdg, compute_reaching_defs};

fn id(s: &str) -> NodeId {
    NodeId::new(s)
}

fn run(src: &str, method_id: &str, tainted_params: &[&str], sinks: &[&str]) -> Phase3Result {
    run_with_sanitizers(src, method_id, tainted_params, sinks, &[])
}

fn run_with_sanitizers(
    src: &str,
    method_id: &str,
    tainted_params: &[&str],
    sinks: &[&str],
    sanitizers: &[&str],
) -> Phase3Result {
    let mid = id(method_id);
    let cfg = build_cfg(&mid, src).expect("CFG must build");
    let dom = cfg.compute_dominators();
    let params: Vec<String> = tainted_params.iter().map(|s| s.to_string()).collect();
    let reaching = compute_reaching_defs(&cfg, &params);
    let pdg = build_pdg(&cfg, Some(&dom), Some(&reaching));
    analyze_with_pdg(&cfg, &pdg, &reaching, &params, sinks, sanitizers)
}

/// Direct: tainted param flows straight into a sink.
#[test]
fn direct_tainted_arg_to_sink() {
    let src = r#"
class Dao {
void query(String input) {
    execute(input);
}
}
"#;
    let r = run(
        src,
        "Method:com.example.Dao#query/1",
        &["input"],
        &["execute"],
    );
    assert!(!r.confirmed_sinks.is_empty(), "should confirm sink");
    assert!(r.confirmed_sinks[0]
        .tainted_args
        .contains(&"input".to_string()));
}

/// Propagation: tainted flows through assignment then into sink.
#[test]
fn taint_propagates_through_assign() {
    let src = r#"
class Dao {
void run(String cmd) {
    String q = cmd;
    exec(q);
}
}
"#;
    let r = run(src, "Method:com.example.Dao#run/1", &["cmd"], &["exec"]);
    assert!(
        !r.confirmed_sinks.is_empty(),
        "should confirm sink via assign chain"
    );
}

/// Kill: reassignment with a literal kills the taint.
#[test]
fn reassignment_kills_taint() {
    let src = r#"
class Dao {
void process(String x) {
    x = "safe";
    execute(x);
}
}
"#;
    let r = run(
        src,
        "Method:com.example.Dao#process/1",
        &["x"],
        &["execute"],
    );
    // After `x = "safe"`, x is no longer tainted.
    // Phase 3 should NOT confirm the sink.
    // (Phase 1 would have confirmed it because x was ever tainted.)
    assert!(
        r.confirmed_sinks.is_empty(),
        "reassignment should kill taint; confirmed_sinks={:?}",
        r.confirmed_sinks
    );
}

/// No taint: untainted param, sink call → multiplier should be 0.60.
#[test]
fn no_taint_low_multiplier() {
    let src = r#"
class Foo {
void safe(String s) {
    execute(s);
}
}
"#;
    let r = run(src, "Method:com.example.Foo#safe/1", &[], &["execute"]);
    assert!(r.confirmed_sinks.is_empty());
    assert!((r.confidence_multiplier - 0.60).abs() < 0.01);
}

/// Return propagation: tainted value returned.
#[test]
fn taint_return_detected() {
    let src = r#"
class Foo {
String get(String input) {
    return input;
}
}
"#;
    let r = run(
        src,
        "Method:com.example.Foo#get/1",
        &["input"],
        &["execute"],
    );
    assert!(r.taint_return, "should detect tainted return");
}

/// Sanitizer kill: result of a sanitizer call is a clean def even if input was tainted.
#[test]
fn sanitizer_kills_taint() {
    let src = r#"
class Web {
void render(String input) {
    String safe = htmlEscape(input);
    print(safe);
}
}
"#;
    // "HtmlUtils#htmlEscape" is the node-id pattern; "htmlEscape" appears in it.
    let r = run_with_sanitizers(
        src,
        "Method:com.example.Web#render/1",
        &["input"],
        &["print"],
        &["HtmlUtils#htmlEscape"],
    );
    assert!(
        r.confirmed_sinks.is_empty(),
        "sanitizer should kill taint; confirmed_sinks={:?}",
        r.confirmed_sinks
    );
    // Without the sanitizer check, the taint would propagate and print would be confirmed.
    // Verify this by running again without the sanitizer pattern — should confirm.
    let r2 = run(
        src,
        "Method:com.example.Web#render/1",
        &["input"],
        &["print"],
    );
    assert!(
        !r2.confirmed_sinks.is_empty(),
        "without sanitizer pattern, print should be confirmed (baseline check)"
    );
}

/// is_sanitizer must NOT match a short call name as a substring of a long pattern.
/// Before the fix, `is_sanitizer("set", &["PreparedStatement#setString"])` returned true,
/// turning a real SQL-injection sink into a sanitizer.
#[test]
fn sanitizer_short_name_does_not_match_longer_pattern() {
    assert!(
        !is_sanitizer("set", &["PreparedStatement#setString"]),
        "'set' must not match pattern 'PreparedStatement#setString'"
    );
    assert!(
        !is_sanitizer("execute", &["Statement#executeQuery"]),
        "'execute' must not match pattern 'Statement#executeQuery'"
    );
    // Exact method-name match still works.
    assert!(
        is_sanitizer("htmlEscape", &["HtmlUtils#htmlEscape"]),
        "'htmlEscape' should match 'HtmlUtils#htmlEscape'"
    );
    assert!(
        is_sanitizer("escapeSql", &["StringEscapeUtils#escapeSql"]),
        "'escapeSql' should match 'StringEscapeUtils#escapeSql'"
    );
}

/// Sink call that is the RHS of an assignment must be detected.
/// Before the fix, `String r = stmt.execute(sql)` (StatementKind::Assign) was silently skipped.
#[test]
fn assign_rhs_sink_detected() {
    let src = r#"
class Dao {
void run(String input) {
    String r = execute(input);
}
}
"#;
    let r = run(
        src,
        "Method:com.example.Dao#run/1",
        &["input"],
        &["execute"],
    );
    assert!(
        !r.confirmed_sinks.is_empty(),
        "sink on assignment RHS should be confirmed; sinks={:?}",
        r.confirmed_sinks
    );
}

/// Sanitizer kill propagates: safe value assigned to another var stays clean.
#[test]
fn sanitizer_kill_propagates() {
    let src = r#"
class Web {
void render(String input) {
    String s1 = htmlEscape(input);
    String s2 = s1;
    sink(s2);
}
}
"#;
    let r = run_with_sanitizers(
        src,
        "Method:com.example.Web#render/1",
        &["input"],
        &["sink"],
        &["HtmlUtils#htmlEscape"],
    );
    assert!(
        r.confirmed_sinks.is_empty(),
        "clean def should propagate through subsequent assignments; sinks={:?}",
        r.confirmed_sinks
    );
}
