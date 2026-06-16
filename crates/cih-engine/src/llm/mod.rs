pub mod anthropic;
pub mod evidence;
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
    match provider {
        "openai-compatible" => Ok(Box::new(openai::OpenAiAdapter::new(base_url))),
        "anthropic" => Ok(Box::new(anthropic::AnthropicAdapter::new(base_url))),
        "http-json" => {
            let config_path = provider_config.ok_or_else(|| {
                anyhow!("--llm-provider http-json requires --llm-provider-config <path>")
            })?;
            Ok(Box::new(http_json::HttpJsonAdapter::load(config_path)?))
        }
        other => bail!(
            "unknown --llm-provider '{}'; expected openai-compatible | anthropic | http-json",
            other
        ),
    }
}

pub fn resolve_api_key(llm_api_key_env: Option<&str>) -> Result<Option<String>> {
    if let Some(var) = llm_api_key_env {
        return Ok(Some(std::env::var(var).map_err(|_| {
            anyhow!("--llm-api-key-env: env var '{}' is unset", var)
        })?));
    }

    Ok(std::env::var("CIH_LLM_API_KEY")
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
    }
}
