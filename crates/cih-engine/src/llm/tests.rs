use super::*;

#[test]
fn make_adapter_rejects_unknown_provider() {
    let err = match make_adapter("unknown", "http://localhost", None) {
        Ok(_) => panic!("unknown provider should fail"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("unknown"));
}

#[test]
fn make_adapter_requires_http_json_config() {
    let err = match make_adapter("http-json", "http://localhost", None) {
        Ok(_) => panic!("missing config should fail"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("--llm-provider-config"));
}

#[test]
fn make_adapter_accepts_builtin_providers() {
    assert!(make_adapter("openai-compatible", "http://localhost", None).is_ok());
    assert!(make_adapter("anthropic", "http://localhost", None).is_ok());
    assert!(make_adapter("deepseek", "http://localhost", None).is_ok());
    assert!(make_adapter("gemini", "http://localhost", None).is_ok());
}

#[test]
fn validate_base_url_accepts_https() {
    assert!(validate_base_url("https://api.openai.com/v1").is_ok());
}

#[test]
fn validate_base_url_accepts_http_localhost() {
    assert!(validate_base_url("http://localhost:11434").is_ok());
    assert!(validate_base_url("http://127.0.0.1:8080/v1").is_ok());
    assert!(validate_base_url("http://[::1]:5000").is_ok());
}

#[test]
fn validate_base_url_rejects_http_remote() {
    let err = validate_base_url("http://example.com/v1")
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTPS"), "expected HTTPS mention: {}", err);
}

#[test]
fn validate_base_url_rejects_non_http_scheme() {
    let err = validate_base_url("ftp://example.com")
        .unwrap_err()
        .to_string();
    assert!(err.contains("https://"), "expected scheme mention: {}", err);
}

#[test]
fn backoff_ms_is_exponential_and_capped() {
    let b0 = backoff_ms(0, 42);
    let b1 = backoff_ms(1, 42);
    let b5 = backoff_ms(5, 42);
    assert!(b1 > b0, "should grow");
    assert!(b5 <= 30_000 + 30_000 / 4 + 1, "should be capped near max");
}

#[test]
fn redact_key_hides_key_in_message() {
    let msg = "error: sk-proj-ABCDEFGHIJK is invalid";
    let redacted = redact_key(msg, Some("sk-proj-ABCDEFGHIJK"));
    assert!(!redacted.contains("ABCDEFGHIJK"), "key should be redacted");
    assert!(redacted.contains("sk-p"), "prefix should survive");
}
