use cih_core::{NodeId, Range};
use cih_taint::{analyze_method, MethodBody, StatementKind, StatementNode};

fn mid(s: &str) -> NodeId {
    NodeId::new(s)
}

fn stmt(
    callable: &NodeId,
    kind: StatementKind,
    reads: &[&str],
    writes: &[&str],
    call_site: Option<&str>,
    call_args: &[&str],
    byte: usize,
) -> StatementNode {
    StatementNode {
        id: NodeId::new(format!("{}:stmt:{byte}", callable.as_str())),
        kind,
        in_callable: callable.clone(),
        range: Range::default(),
        reads: reads.iter().map(|s| s.to_string()).collect(),
        writes: writes.iter().map(|s| s.to_string()).collect(),
        call_site: call_site.map(str::to_string),
        call_args: call_args.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn confirms_direct_tainted_sink_call() {
    let id = mid("Method:com.example.Foo#process/1");
    let body = MethodBody {
        callable_id: id.clone(),
        param_names: vec!["input".to_string()],
        statements: vec![stmt(&id, StatementKind::Call, &[], &[], Some("execute"), &["input"], 10)],
    };

    let result = analyze_method(&body, &["input".to_string()], &["execute"]);
    assert_eq!(result.confirmed_sinks.len(), 1);
    assert_eq!(result.confirmed_sinks[0].call_name, "execute");
    assert!(result.confirmed_sinks[0].tainted_args.contains(&"input".to_string()));
}

#[test]
fn propagates_taint_through_assign_then_sink() {
    let id = mid("Method:com.example.Foo#process/1");
    let body = MethodBody {
        callable_id: id.clone(),
        param_names: vec!["input".to_string()],
        statements: vec![
            stmt(&id, StatementKind::Assign, &["input"], &["q"], Some("build"), &["input"], 10),
            stmt(&id, StatementKind::Call, &[], &[], Some("execute"), &["q"], 20),
        ],
    };

    let result = analyze_method(&body, &["input".to_string()], &["execute"]);
    assert_eq!(result.confirmed_sinks.len(), 1);
    assert!(result.confirmed_sinks[0].tainted_args.contains(&"q".to_string()));
}

#[test]
fn no_taint_no_sink_confirmation() {
    let id = mid("Method:com.example.Foo#process/1");
    let body = MethodBody {
        callable_id: id.clone(),
        param_names: vec!["input".to_string()],
        statements: vec![stmt(
            &id,
            StatementKind::Call,
            &[],
            &[],
            Some("execute"),
            &["hardcoded"],
            10,
        )],
    };

    let result = analyze_method(&body, &["input".to_string()], &["execute"]);
    assert!(result.confirmed_sinks.is_empty(), "hardcoded arg should not confirm sink");
}

#[test]
fn taint_return_detected() {
    let id = mid("Method:com.example.Foo#process/1");
    let body = MethodBody {
        callable_id: id.clone(),
        param_names: vec!["input".to_string()],
        statements: vec![stmt(&id, StatementKind::Return, &["input"], &[], None, &[], 10)],
    };

    let result = analyze_method(&body, &["input".to_string()], &["execute"]);
    assert!(result.taint_return, "should detect taint reaching return");
}
