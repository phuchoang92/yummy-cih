use anyhow::{Context, Result};

use super::{require_api_key, LlmAdapter, LlmRequest, LlmResponse};

pub struct OpenAiAdapter {
    base_url: String,
}

impl OpenAiAdapter {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

impl LlmAdapter for OpenAiAdapter {
    fn call(&self, api_key: Option<&str>, req: &LlmRequest) -> Result<LlmResponse> {
        let api_key = require_api_key(api_key, "openai-compatible")?;
        let url = format!("{}/chat/completions", self.base_url);
        let mut messages = Vec::new();
        if !req.system.trim().is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": req.system}));
        }
        messages.push(serde_json::json!({"role": "user", "content": req.user}));
        let body = serde_json::json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": messages
        });

        let response = match ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", api_key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(req.timeout_secs))
            .send_json(body)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                anyhow::bail!(
                    "OpenAI-compatible API HTTP {}: {}",
                    status,
                    &body[..body.len().min(1000)]
                );
            }
            Err(err) => return Err(anyhow::anyhow!(err).context("OpenAI-compatible API request failed")),
        };

        let resp: serde_json::Value = response
            .into_json()
            .context("failed to parse OpenAI-compatible API response")?;

        let text = resp["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .with_context(|| format!("unexpected OpenAI-compatible response shape: {:?}", resp))?;

        Ok(LlmResponse { text })
    }
}
