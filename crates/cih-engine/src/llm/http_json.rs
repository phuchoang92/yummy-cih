use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use super::{LlmAdapter, LlmRequest, LlmResponse};

#[derive(Clone, Debug, Deserialize)]
pub struct HttpJsonConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub body_template: serde_json::Value,
    #[serde(default)]
    pub response_path: String,
}

#[derive(Debug)]
pub struct HttpJsonAdapter {
    config: HttpJsonConfig,
}

impl HttpJsonAdapter {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: HttpJsonConfig = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if config.url.trim().is_empty() {
            bail!("http-json config requires non-empty url");
        }
        if config.response_path.trim().is_empty() {
            bail!("http-json config requires non-empty response_path");
        }
        Ok(Self { config })
    }

    pub fn config(&self) -> &HttpJsonConfig {
        &self.config
    }

    fn render_headers(&self, api_key: Option<&str>) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        for (k, v) in &self.config.headers {
            out.insert(k.clone(), render_string(v, api_key, None)?);
        }
        Ok(out)
    }

    fn render_body(&self, api_key: Option<&str>, req: &LlmRequest) -> Result<serde_json::Value> {
        render_value(&self.config.body_template, api_key, Some(req))
    }
}

impl LlmAdapter for HttpJsonAdapter {
    fn call(&self, api_key: Option<&str>, req: &LlmRequest) -> Result<LlmResponse> {
        let headers = self.render_headers(api_key)?;
        let body = self.render_body(api_key, req)?;

        let mut request =
            ureq::post(&self.config.url).timeout(std::time::Duration::from_secs(req.timeout_secs));
        for (k, v) in headers {
            request = request.set(&k, &v);
        }
        let response = request
            .send_json(body)
            .context("http-json API request failed")?;
        let resp: serde_json::Value = response
            .into_json()
            .context("failed to parse http-json API response")?;
        let value = resolve_path(&resp, &self.config.response_path)
            .with_context(|| format!("response_path '{}' not found", self.config.response_path))?;
        let text = value
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| value.to_string());
        Ok(LlmResponse { text })
    }
}

fn render_value(
    value: &serde_json::Value,
    api_key: Option<&str>,
    req: Option<&LlmRequest>,
) -> Result<serde_json::Value> {
    match value {
        serde_json::Value::String(s) => {
            if s == "{{max_tokens}}" {
                let req = req.ok_or_else(|| anyhow!("{{max_tokens}} requires an LLM request"))?;
                return Ok(serde_json::Value::Number(req.max_tokens.into()));
            }
            Ok(serde_json::Value::String(render_string(s, api_key, req)?))
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(render_value(item, api_key, req)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), render_value(v, api_key, req)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn render_string(s: &str, api_key: Option<&str>, req: Option<&LlmRequest>) -> Result<String> {
    let mut out = s.to_string();
    if out.contains("{{api_key}}") {
        let key = api_key.ok_or_else(|| {
            anyhow!("http-json config uses {{api_key}} but no API key was resolved")
        })?;
        out = out.replace("{{api_key}}", key);
    }
    if let Some(req) = req {
        out = out.replace("{{prompt}}", &req.user);
        out = out.replace("{{system}}", &req.system);
        out = out.replace("{{model}}", &req.model);
        out = out.replace("{{max_tokens}}", &req.max_tokens.to_string());
    }
    replace_env_placeholders(&out)
}

fn replace_env_placeholders(input: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("{{env:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{{env:".len()..];
        let Some(end) = after.find("}}") else {
            bail!("unterminated {{env:...}} placeholder");
        };
        let var = &after[..end];
        if var.trim().is_empty() {
            bail!("empty {{env:...}} placeholder");
        }
        let value = std::env::var(var).with_context(|| {
            format!("http-json config uses {{env:{}}} but env var is unset", var)
        })?;
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        if let Ok(idx) = segment.parse::<usize>() {
            current = current.as_array()?.get(idx)?;
        } else {
            current = current.as_object()?.get(segment)?;
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
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
}
