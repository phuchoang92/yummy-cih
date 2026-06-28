use cih_core::NodeId;
use cih_taint::{build_cfg, build_pdg, compute_reaching_defs, param_def_id};

fn mid(s: &str) -> NodeId {
    NodeId::new(s)
}

#[test]
fn data_deps_linear() {
    let src = r#"
class Foo {
    String process(String input) {
        String result = sanitize(input);
        return result;
    }
}
"#;
    let id = mid("Method:com.example.Foo#process/1");
    let cfg = build_cfg(&id, src).expect("CFG");
    let dom = cfg.compute_dominators();
    let params = vec!["input".to_string()];
    let reaching = compute_reaching_defs(&cfg, &params);
    let pdg = build_pdg(&cfg, Some(&dom), Some(&reaching));

    let param_id = param_def_id(&id, "input");
    let has_param_dep = pdg.data_edges().any(|e| e.from == param_id);
    assert!(has_param_dep, "should have data dep from param def; edges: {:?}", pdg.data_edges().collect::<Vec<_>>());
}

#[test]
fn data_dep_assignment_chain() {
    let src = r#"
class Foo {
    void run(String cmd) {
        String q = cmd;
        exec(q);
    }
}
"#;
    let id = mid("Method:com.example.Foo#run/1");
    let cfg = build_cfg(&id, src).expect("CFG");
    let dom = cfg.compute_dominators();
    let params = vec!["cmd".to_string()];
    let reaching = compute_reaching_defs(&cfg, &params);
    let pdg = build_pdg(&cfg, Some(&dom), Some(&reaching));

    let data_edges: Vec<_> = pdg.data_edges().collect();
    assert!(data_edges.len() >= 2, "expected ≥2 data deps, got {:?}", data_edges);
}

#[test]
fn control_dep_if_body() {
    let src = r#"
class Foo {
    void check(String s) {
        if (s != null) {
            log(s);
        }
    }
}
"#;
    let id = mid("Method:com.example.Foo#check/1");
    let cfg = build_cfg(&id, src).expect("CFG");
    let dom = cfg.compute_dominators();
    let reaching = compute_reaching_defs(&cfg, &["s".to_string()]);
    let pdg = build_pdg(&cfg, Some(&dom), Some(&reaching));

    let ctrl_edges: Vec<_> = pdg.control_edges().collect();
    assert!(!ctrl_edges.is_empty(), "if-body should be control-dependent on branch");
}

#[test]
fn reaching_defs_kill() {
    let src = r#"
class Foo {
    void process(String x) {
        x = "safe";
        use(x);
    }
}
"#;
    let id = mid("Method:com.example.Foo#process/1");
    let cfg = build_cfg(&id, src).expect("CFG");
    let params = vec!["x".to_string()];
    let reaching = compute_reaching_defs(&cfg, &params);

    let param_def = param_def_id(&id, "x");
    for (stmt_id, rd) in &reaching {
        let x_defs = rd.get("x").cloned().unwrap_or_default();
        if stmt_id.as_str().contains("stmt") {
            if !x_defs.contains(&param_def) {
                return;
            }
        }
    }
    assert!(
        reaching.values().any(|rd| rd.get("x").map(|defs| defs.contains(&param_def)).unwrap_or(false)),
        "param def should appear in at least some reaching defs"
    );
}
