use cih_core::NodeId;
use cih_taint::{analyze_with_pdg, build_cfg, build_pdg, compute_reaching_defs, PdgResult};

fn id(s: &str) -> NodeId {
    NodeId::new(s)
}

fn run(src: &str, method_id: &str, tainted_params: &[&str], sinks: &[&str]) -> PdgResult {
    run_with_sanitizers(src, method_id, tainted_params, sinks, &[])
}

fn run_with_sanitizers(
    src: &str,
    method_id: &str,
    tainted_params: &[&str],
    sinks: &[&str],
    sanitizers: &[&str],
) -> PdgResult {
    let mid = id(method_id);
    let cfg = build_cfg(&mid, src).expect("CFG must build");
    let dom = cfg.compute_dominators();
    let params: Vec<String> = tainted_params.iter().map(|s| s.to_string()).collect();
    let reaching = compute_reaching_defs(&cfg, &params);
    let pdg = build_pdg(&cfg, Some(&dom), Some(&reaching));
    analyze_with_pdg(&cfg, &pdg, &reaching, &params, sinks, sanitizers)
}

#[test]
fn direct_tainted_arg_to_sink() {
    let src = r#"
class Dao {
    void query(String input) {
        execute(input);
    }
}
"#;
    let r = run(src, "Method:com.example.Dao#query/1", &["input"], &["execute"]);
    assert!(!r.confirmed_sinks.is_empty(), "should confirm sink");
    assert!(r.confirmed_sinks[0].tainted_args.contains(&"input".to_string()));
}

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
    assert!(!r.confirmed_sinks.is_empty(), "should confirm sink via assign chain");
}

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
    let r = run(src, "Method:com.example.Dao#process/1", &["x"], &["execute"]);
    assert!(
        r.confirmed_sinks.is_empty(),
        "reassignment should kill taint; confirmed_sinks={:?}",
        r.confirmed_sinks
    );
}

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

#[test]
fn taint_return_detected() {
    let src = r#"
class Foo {
    String get(String input) {
        return input;
    }
}
"#;
    let r = run(src, "Method:com.example.Foo#get/1", &["input"], &["execute"]);
    assert!(r.taint_return, "should detect tainted return");
}

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
    let r2 = run(src, "Method:com.example.Web#render/1", &["input"], &["print"]);
    assert!(
        !r2.confirmed_sinks.is_empty(),
        "without sanitizer pattern, print should be confirmed (baseline check)"
    );
}

#[test]
fn sanitizer_short_name_does_not_match_longer_pattern() {
    // "set" must NOT match "PreparedStatement#setString" — if it did, taint would be killed
    // before reaching execute(input) and the sink would not be confirmed.
    let src = r#"
class Dao {
    void run(String input) {
        set(input);
        execute(input);
    }
}
"#;
    let r = run_with_sanitizers(
        src,
        "Method:com.example.Dao#run/1",
        &["input"],
        &["execute"],
        &["PreparedStatement#setString"],
    );
    assert!(
        !r.confirmed_sinks.is_empty(),
        "'set' matched 'PreparedStatement#setString' as sanitizer — execute should still be tainted"
    );
}

#[test]
fn assign_rhs_sink_detected() {
    let src = r#"
class Dao {
    void run(String input) {
        String r = execute(input);
    }
}
"#;
    let r = run(src, "Method:com.example.Dao#run/1", &["input"], &["execute"]);
    assert!(
        !r.confirmed_sinks.is_empty(),
        "sink on assignment RHS should be confirmed; sinks={:?}",
        r.confirmed_sinks
    );
}

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
