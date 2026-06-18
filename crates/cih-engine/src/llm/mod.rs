pub mod anthropic;
pub mod evidence;
pub mod grouping;
pub mod http_json;
pub mod openai;

use std::path::Path;

use anyhow::{anyhow, bail, Result};

pub struct LlmRequest {
    pub system: String,
    pub user: String,
    pub model: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
}

pub struct LlmResponse {
    pub text: String,
}

pub trait LlmAdapter: Send + Sync {
    fn call(&self, api_key: Option<&str>, req: &LlmRequest) -> Result<LlmResponse>;
}

pub fn make_adapter(
    provider: &str,
    base_url: &str,
    provider_config: Option<&Path>,
) -> Result<Box<dyn LlmAdapter>> {
    validate_base_url(base_url)?;
    match provider {
        "openai-compatible" => Ok(Box::new(openai::OpenAiAdapter::new(base_url))),
        "anthropic" => Ok(Box::new(anthropic::AnthropicAdapter::new(base_url))),
        "deepseek" => Ok(Box::new(openai::OpenAiAdapter::new("https://api.deepseek.com"))),
        "gemini" => Ok(Box::new(openai::OpenAiAdapter::new(
            "https://generativelanguage.googleapis.com/v1beta/openai",
        ))),
        "http-json" => {
            let config_path = provider_config.ok_or_else(|| {
                anyhow!("--llm-provider http-json requires --llm-provider-config <path>")
            })?;
            Ok(Box::new(http_json::HttpJsonAdapter::load(config_path)?))
        }
        other => bail!(
            "unknown --llm-provider '{}'; expected openai-compatible | anthropic | deepseek | gemini | http-json",
            other
        ),
    }
}

/// Require HTTPS for remote URLs; allow HTTP for localhost and loopback only.
fn validate_base_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://") {
        let rest = &url["http://".len()..];
        let authority = rest.split('/').next().unwrap_or("");
        // IPv6 literal: http://[::1]:5000 → strip brackets to get ::1
        let host = if authority.starts_with('[') {
            authority
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or("")
        } else {
            authority.split(':').next().unwrap_or("")
        };
        if matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return Ok(());
        }
        bail!(
            "LLM base URL '{}' uses HTTP for a non-local host; use HTTPS for remote endpoints",
            url
        );
    }
    bail!("LLM base URL '{}' must start with https:// or http://", url)
}

/// Exponential backoff with deterministic jitter (no thread-rng dependency).
/// Returns milliseconds to sleep before `attempt` (0-indexed).
pub fn backoff_ms(attempt: usize, jitter_seed: u64) -> u64 {
    const BASE_MS: u64 = 500;
    const MAX_MS: u64 = 30_000;
    let exp = BASE_MS.saturating_mul(1u64 << attempt.min(10));
    let capped = exp.min(MAX_MS);
    // LCG step for deterministic jitter
    let j = jitter_seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let jitter = j % (capped / 4 + 1);
    capped + jitter
}

/// Redact an API key from an error string for safe logging.
pub fn redact_key(msg: &str, key: Option<&str>) -> String {
    let Some(k) = key else { return msg.to_string() };
    if k.len() < 8 {
        return msg.replace(k, "[REDACTED]");
    }
    let visible = &k[..4];
    msg.replace(k, &format!("{}…[REDACTED]", visible))
}

pub fn resolve_api_key(llm_api_key_env: Option<&str>) -> Result<Option<String>> {
    if let Some(var) = llm_api_key_env {
        return Ok(Some(std::env::var(var).map_err(|_| {
            anyhow!("--llm-api-key-env: env var '{}' is unset", var)
        })?));
    }

    Ok(std::env::var("CIH_LLM_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .or_else(|_| std::env::var("GEMINI_API_KEY"))
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .ok())
}

pub fn require_api_key<'a>(api_key: Option<&'a str>, provider: &str) -> Result<&'a str> {
    api_key.ok_or_else(|| {
        anyhow!(
            "--llm-provider {} requires an API key in CIH_LLM_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, or --llm-api-key-env",
            provider
        )
    })
}

#[cfg(test)]
mod tests {
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
}
