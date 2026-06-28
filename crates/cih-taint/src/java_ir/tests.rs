use super::*;

fn mid(s: &str) -> NodeId {
    NodeId::new(s)
}

#[test]
fn parse_simple_method() {
    let src = r#"
class Foo {
public String process(String input) {
    String result = sanitize(input);
    return result;
}
}
"#;
    let id = mid("Method:com.example.Foo#process/1");
    let body = extract_method_body(&id, src).expect("should find method");
    assert_eq!(body.param_names, vec!["input"]);
    assert!(
        body.statements.len() >= 2,
        "expected at least 2 stmts, got {}",
        body.statements.len()
    );
    let assign = &body.statements[0];
    assert_eq!(assign.kind, StatementKind::Assign);
    assert!(assign.writes.contains(&"result".to_string()));
    assert!(assign.reads.contains(&"input".to_string()));
    let ret = &body.statements[1];
    assert_eq!(ret.kind, StatementKind::Return);
    assert!(ret.reads.contains(&"result".to_string()));
}

#[test]
fn parse_method_not_found_returns_none() {
    let src = r#"class Foo { void bar() {} }"#;
    let id = mid("Method:com.example.Foo#nonexistent/0");
    assert!(extract_method_body(&id, src).is_none());
}

#[test]
fn parse_if_and_call() {
    let src = r#"
class OrderService {
void save(String query) {
    if (query != null) {
        jdbcTemplate.execute(query);
    }
}
}
"#;
    let id = mid("Method:com.example.OrderService#save/1");
    let body = extract_method_body(&id, src).expect("should find method");
    assert_eq!(body.param_names, vec!["query"]);

    let branch = body
        .statements
        .iter()
        .find(|s| s.kind == StatementKind::Branch)
        .expect("expected a Branch statement");
    assert!(branch.reads.contains(&"query".to_string()));

    let call = body
        .statements
        .iter()
        .find(|s| s.kind == StatementKind::Call)
        .expect("expected a Call statement");
    assert_eq!(call.call_site.as_deref(), Some("execute"));
    assert!(call.call_args.contains(&"query".to_string()));
}
