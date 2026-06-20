use super::*;

#[test]
fn builtin_defaults_cover_all_three_languages() {
    let reg = EntrypointRegistry::builtin_defaults();
    // Java Spring
    assert!(reg.http.contains("GetMapping"));
    assert!(reg.http.contains("POST"));
    // TypeScript NestJS
    assert!(reg.http.contains("Get"));
    assert!(reg.event.contains("MessagePattern"));
    // Python Flask/FastAPI
    assert!(reg.http.contains("app.route"));
    assert!(reg.http.contains("router.get"));
    assert!(reg.event.contains("task"));
    // Scheduled
    assert!(reg.scheduled.contains("Scheduled"));
    assert!(reg.scheduled.contains("Cron"));
}

#[test]
fn merge_toml_adds_custom_patterns() {
    let mut reg = EntrypointRegistry::default();
    let toml = r#"
[http]
annotations = ["MyRoute", "MyGet"]

[event]
annotations = ["MyListener"]

[scheduled]
annotations = ["MyCron"]
"#;
    reg.merge_toml(toml);
    assert!(reg.http.contains("MyRoute"));
    assert!(reg.http.contains("MyGet"));
    assert!(reg.event.contains("MyListener"));
    assert!(reg.scheduled.contains("MyCron"));
}

#[test]
fn malformed_toml_is_ignored() {
    let mut reg = EntrypointRegistry::builtin_defaults();
    let before = reg.total_patterns();
    reg.merge_toml("this is not valid { toml !!!!");
    assert_eq!(reg.total_patterns(), before);
}
