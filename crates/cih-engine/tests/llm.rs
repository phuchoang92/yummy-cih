use cih_engine_lib::llm::*;

#[test]
fn llm_provider_rejects_unknown_string() {
    let err = "unknown".parse::<LlmProvider>().unwrap_err().to_string();
    assert!(err.contains("unknown"));
}

#[test]
fn make_adapter_requires_http_json_config() {
    let err = match make_adapter(&LlmProvider::HttpJson, "http://localhost", None) {
        Ok(_) => panic!("missing config should fail"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("--llm-provider-config"));
}

#[test]
fn make_adapter_accepts_builtin_providers() {
    assert!(make_adapter(&LlmProvider::OpenAiCompatible, "http://localhost", None).is_ok());
    assert!(make_adapter(&LlmProvider::Anthropic, "http://localhost", None).is_ok());
    assert!(make_adapter(&LlmProvider::Bedrock, "https://bedrock-runtime.us-east-1.amazonaws.com", None).is_ok());
    assert!(make_adapter(&LlmProvider::DeepSeek, "http://localhost", None).is_ok());
    assert!(make_adapter(&LlmProvider::Gemini, "http://localhost", None).is_ok());
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

/// Integration test: verifies that the Bedrock Converse API adapter works end-to-end.
/// Only runs when AWS_BEARER_TOKEN_BEDROCK is set in the environment.
#[test]
fn integration_bedrock_community_full_prompt() {
    let api_key = match std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
        Ok(k) if !k.is_empty() => k,
        _ => return, // skip when key not set
    };
    let base_url = std::env::var("AWS_BEDROCK_BASE_URL")
        .unwrap_or_else(|_| "https://bedrock-runtime.us-east-1.amazonaws.com".to_string());

    use cih_engine_lib::llm::{make_adapter, LlmProvider, LlmRequest};
    use cih_engine_lib::llm::prompts::{community_system, COMMUNITY_FULL_JSON_TEMPLATE};

    let adapter = make_adapter(&LlmProvider::Bedrock, &base_url, None).expect("Bedrock adapter");

    let system = community_system("en");
    let user = format!(
        "Module: \"PaymentService\"\n\nEvidence:\n[R1] POST /api/payments\n[S1] processPayment(){{validateCard(); gateway.charge();}}\n\n{}",
        COMMUNITY_FULL_JSON_TEMPLATE
    );
    let req = LlmRequest {
        system,
        user,
        model: "us.anthropic.claude-haiku-4-5-20251001".into(),
        max_tokens: 2000,
        timeout_secs: 60,
    };

    let resp = adapter.call(Some(&api_key), &req).expect("Bedrock call");
    let text = &resp.text;

    let (s, e) = (text.find('{'), text.rfind('}'));
    let json_str = match (s, e) {
        (Some(s), Some(e)) if s < e => &text[s..=e],
        _ => text.as_str(),
    };
    let val: serde_json::Value = serde_json::from_str(json_str)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nRaw: {}", &text[..text.len().min(300)]));

    for field in &["po_summary", "po_capabilities", "ba_process_overview", "dev_entry_points"] {
        assert!(
            val[field].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "field '{}' missing or empty in response",
            field
        );
    }
}

/// Integration test: verifies that prompt constants produce valid JSON via DeepSeek.
/// Only runs when DEEPSEEK_API_KEY is set in the environment.
#[test]
fn integration_deepseek_community_full_prompt() {
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return, // skip when key not set
    };

    use cih_engine_lib::llm::{make_adapter, LlmProvider, LlmRequest};
    use cih_engine_lib::llm::prompts::{community_system, COMMUNITY_FULL_JSON_TEMPLATE};

    let adapter = make_adapter(&LlmProvider::DeepSeek, "https://api.deepseek.com", None)
        .expect("DeepSeek adapter");

    let system = community_system("en");
    let user = format!(
        "Module: \"PaymentService\"\n\nEvidence:\n[R1] POST /api/payments\n[S1] processPayment(){{validateCard(); gateway.charge();}}\n\n{}",
        COMMUNITY_FULL_JSON_TEMPLATE
    );
    let req = LlmRequest { system, user, model: "deepseek-chat".into(), max_tokens: 2000, timeout_secs: 30 };

    let resp = adapter.call(Some(&api_key), &req).expect("DeepSeek call");
    let text = &resp.text;

    // Same extraction logic as parse_llm_full
    let (s, e) = (text.find('{'), text.rfind('}'));
    let json_str = match (s, e) {
        (Some(s), Some(e)) if s < e => &text[s..=e],
        _ => text.as_str(),
    };
    let val: serde_json::Value = serde_json::from_str(json_str)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nRaw: {}", &text[..text.len().min(300)]));

    for field in &["po_summary", "po_capabilities", "ba_process_overview", "dev_entry_points"] {
        assert!(
            val[field].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "field '{}' missing or empty in response", field
        );
    }
}

#[test]
fn integration_deepseek_http_flow_prompt() {
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return,
    };

    use cih_engine_lib::llm::{make_adapter, LlmProvider, LlmRequest};
    use cih_engine_lib::llm::prompts::{http_flow_system, HTTP_FLOW_JSON_TEMPLATE};

    let adapter = make_adapter(&LlmProvider::DeepSeek, "https://api.deepseek.com", None)
        .expect("DeepSeek adapter");

    let step_count = 3usize;
    let system = http_flow_system("en");
    let json_template = HTTP_FLOW_JSON_TEMPLATE.replace("{step_count}", &step_count.to_string());
    let user = format!(
        "HTTP handler: \"processPayment\"\n\nCall chain (3 steps):\n[1] PaymentController.processPayment() (Controller)\n[2] PaymentService.validate() (Service)\n[3] GatewayClient.charge() (Client)\n\n{}",
        json_template
    );
    let req = LlmRequest { system, user, model: "deepseek-chat".into(), max_tokens: 600, timeout_secs: 30 };

    let resp = adapter.call(Some(&api_key), &req).expect("DeepSeek call");
    let text = &resp.text;

    let (s, e) = (text.find('{'), text.rfind('}'));
    let json_str = match (s, e) {
        (Some(s), Some(e)) if s < e => &text[s..=e],
        _ => text.as_str(),
    };
    let val: serde_json::Value = serde_json::from_str(json_str)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\nRaw: {}", &text[..text.len().min(300)]));

    assert!(val["narrative"].as_str().map(|s| !s.is_empty()).unwrap_or(false), "narrative missing");
    assert!(val["business_impact"].as_str().map(|s| !s.is_empty()).unwrap_or(false), "business_impact missing");
    let descs = val["step_descriptions"].as_array().expect("step_descriptions should be array");
    assert!(!descs.is_empty(), "step_descriptions should not be empty");
}
