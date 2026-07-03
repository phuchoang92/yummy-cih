use super::*;

#[test]
fn missing_file_returns_defaults() {
    let tmp = std::env::temp_dir().join("cih-taint-cfg-test-missing");
    std::fs::create_dir_all(&tmp).unwrap();
    let rules = load_taint_rules(&tmp);
    assert!(!rules.sinks.is_empty(), "defaults must have sinks");
    assert!(!rules.extra_sink_name_patterns.is_empty(), "defaults must have name patterns");
}

#[test]
fn custom_sink_merged_with_defaults() {
    let tmp = std::env::temp_dir().join("cih-taint-cfg-test-custom");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("cih.taint.toml"),
        r#"
[[sink]]
pattern = "MyDao#customExecute"
category = "sql"

[[sanitizer]]
pattern = "MyValidator#sanitize"
"#,
    )
    .unwrap();

    let rules = load_taint_rules(&tmp);

    assert!(
        rules.sinks.iter().any(|s| s.node_id_pattern == "MyDao#customExecute"),
        "custom sink not found in merged rules"
    );
    assert!(
        rules.sinks.iter().any(|s| s.node_id_pattern == "Runtime#exec"),
        "default sink missing after merge"
    );
    assert!(
        rules.sanitizers.iter().any(|s| s.node_id_pattern == "MyValidator#sanitize"),
        "custom sanitizer not found"
    );
    assert!(
        rules.extra_sink_name_patterns.iter().any(|p| p == "customExecute"),
        "method name not extracted into extra_sink_name_patterns"
    );
}

#[test]
fn extend_defaults_false_replaces_defaults() {
    let tmp = std::env::temp_dir().join("cih-taint-cfg-test-replace");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("cih.taint.toml"),
        r#"
[settings]
extend_defaults = false

[[sink]]
pattern = "OnlySink#run"
"#,
    )
    .unwrap();

    let rules = load_taint_rules(&tmp);
    assert_eq!(rules.sinks.len(), 1, "only the custom sink should be present");
    assert_eq!(rules.sinks[0].node_id_pattern, "OnlySink#run");
}
