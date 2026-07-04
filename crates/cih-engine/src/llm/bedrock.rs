use anyhow::{Context, Result};
use std::sync::mpsc;
use std::time::Duration;

use super::{require_api_key, LlmAdapter, LlmRequest, LlmResponse};

pub struct BedrockAdapter {
    base_url: String,
}

impl BedrockAdapter {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

impl LlmAdapter for BedrockAdapter {
    fn call(&self, api_key: Option<&str>, req: &LlmRequest) -> Result<LlmResponse> {
        let api_key = require_api_key(api_key, "bedrock")?;
        // Bedrock Converse API embeds the model in the URL, not the request body.
        let url = format!("{}/model/{}/converse", self.base_url, req.model);
        let mut body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"text": req.user}]}],
            "inferenceConfig": {"maxTokens": req.max_tokens}
        });
        if !req.system.trim().is_empty() {
            body["system"] = serde_json::json!([{"text": req.system}]);
        }

        let key = api_key.to_string();
        let timeout = Duration::from_secs(req.timeout_secs);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(do_call(url, key, body));
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                anyhow::bail!("Bedrock request timed out after {}s", timeout.as_secs())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("Bedrock request thread panicked before responding")
            }
        }
    }
}

fn do_call(url: String, api_key: String, body: serde_json::Value) -> Result<LlmResponse> {
    let response = match ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", api_key))
        .set("Content-Type", "application/json")
        .send_json(body)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(status, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow::bail!(
                "Bedrock API HTTP {}: {}",
                status,
                &body[..body.len().min(1000)]
            );
        }
        Err(err) => return Err(anyhow::anyhow!(err).context("Bedrock API request failed")),
    };

    let resp: serde_json::Value = response
        .into_json()
        .context("failed to parse Bedrock API response")?;

    let text = resp["output"]["message"]["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .with_context(|| format!("unexpected Bedrock response shape: {:?}", resp))?;

    Ok(LlmResponse { text })
}
