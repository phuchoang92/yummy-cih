use super::*;
use crate::llm::LlmRequest;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn temp_config(content: &str) -> std::path::PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "cih-http-json-config-{}-{id}.json",
        std::process::id()
    ));
    std::fs::write(&path, content).unwrap();
    path
}

fn request() -> LlmRequest {
    LlmRequest {
        system: "system text".into(),
        user: "hello \"world\"\nnext".into(),
        model: "local-model".into(),
        max_tokens: 123,
        timeout_secs: 1,
    }
}

#[test]
fn load_valid_config() {
    let path = temp_config(
        r#"{
          "url": "http://localhost:11434/api/generate",
          "headers": {"Content-Type": "application/json"},
          "body_template": {"model": "{{model}}", "prompt": "{{prompt}}"},
          "response_path": "response"
        }"#,
    );
    let adapter = HttpJsonAdapter::load(&path).unwrap();
    assert_eq!(adapter.config().url, "http://localhost:11434/api/generate");
    assert_eq!(adapter.config().response_path, "response");
    let _ = std::fs::remove_file(path);
}

#[test]
fn load_missing_response_path_errors() {
    let path = temp_config(
        r#"{
          "url": "http://localhost",
          "body_template": {"prompt": "{{prompt}}"}
        }"#,
    );
    let err = HttpJsonAdapter::load(&path).unwrap_err().to_string();
    assert!(err.contains("response_path"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn response_path_extracts_nested_value() {
    let value = serde_json::json!({"choices": [{"message": {"content": "ok"}}]});
    assert_eq!(
        resolve_path(&value, "choices.0.message.content").and_then(|v| v.as_str()),
        Some("ok")
    );
    assert!(resolve_path(&value, "choices.1.message.content").is_none());
}

#[test]
fn render_body_substitutes_json_safely_and_keeps_numeric_max_tokens() {
    let config = HttpJsonConfig {
        url: "http://localhost".into(),
        headers: HashMap::new(),
        body_template: serde_json::json!({
            "model": "{{model}}",
            "prompt": "{{prompt}}",
            "max_tokens": "{{max_tokens}}"
        }),
        response_path: "response".into(),
    };
    let adapter = HttpJsonAdapter { config };
    let body = adapter.render_body(None, &request()).unwrap();
    assert_eq!(body["model"], "local-model");
    assert_eq!(body["prompt"], "hello \"world\"\nnext");
    assert_eq!(body["max_tokens"], 123);
}

#[test]
fn api_key_placeholder_requires_key() {
    let config = HttpJsonConfig {
        url: "http://localhost".into(),
        headers: HashMap::from([("Authorization".into(), "Bearer {{api_key}}".into())]),
        body_template: serde_json::json!({"prompt": "{{prompt}}"}),
        response_path: "response".into(),
    };
    let adapter = HttpJsonAdapter { config };
    let err = adapter.render_headers(None).unwrap_err().to_string();
    assert!(err.contains("api_key"));
    let headers = adapter.render_headers(Some("secret")).unwrap();
    assert_eq!(headers["Authorization"], "Bearer secret");
}

#[test]
fn env_placeholder_requires_env_var() {
    let err = replace_env_placeholders("Bearer {{env:CIH_TEST_MISSING_TOKEN}}")
        .unwrap_err()
        .to_string();
    assert!(err.contains("CIH_TEST_MISSING_TOKEN"));
}
