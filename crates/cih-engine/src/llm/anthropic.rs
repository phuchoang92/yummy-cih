use anyhow::{Context, Result};

use super::{require_api_key, LlmAdapter, LlmRequest, LlmResponse};

pub struct AnthropicAdapter {
    base_url: String,
}

impl AnthropicAdapter {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

impl LlmAdapter for AnthropicAdapter {
    fn call(&self, api_key: Option<&str>, req: &LlmRequest) -> Result<LlmResponse> {
        let api_key = require_api_key(api_key, "anthropic")?;
        let url = format!("{}/messages", self.base_url);
        let mut body = serde_json::json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": [{"role": "user", "content": req.user}]
        });
        if !req.system.trim().is_empty() {
            body["system"] = serde_json::Value::String(req.system.clone());
        }

        let response = ureq::post(&url)
            .set("x-api-key", api_key)
            .set("anthropic-version", "2023-06-01")
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(req.timeout_secs))
            .send_json(body)
            .context("Anthropic API request failed")?;

        let resp: serde_json::Value = response
            .into_json()
            .context("failed to parse Anthropic API response")?;

        let text = resp["content"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
            .with_context(|| format!("unexpected Anthropic response shape: {:?}", resp))?;

        Ok(LlmResponse { text })
    }
}
