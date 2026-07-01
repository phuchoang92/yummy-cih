pub mod anthropic;
pub mod prompts;
pub mod evidence;
pub mod grouping;
pub mod http_json;
pub mod openai;

use std::path::Path;

use anyhow::{anyhow, bail, Result};

/// LLM backend provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LlmProvider {
    #[default]
    OpenAiCompatible,
    Anthropic,
    DeepSeek,
    Gemini,
    HttpJson,
}

impl std::fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
            Self::DeepSeek => "deepseek",
            Self::Gemini => "gemini",
            Self::HttpJson => "http-json",
        })
    }
}

impl std::str::FromStr for LlmProvider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "openai-compatible" => Ok(Self::OpenAiCompatible),
            "anthropic" => Ok(Self::Anthropic),
            "deepseek" => Ok(Self::DeepSeek),
            "gemini" => Ok(Self::Gemini),
            "http-json" => Ok(Self::HttpJson),
            other => bail!(
                "unknown --llm-provider '{}'; expected openai-compatible | anthropic | deepseek | gemini | http-json",
                other
            ),
        }
    }
}

/// Shared LLM call configuration used by WikiConfig and the feature-classification stage.
pub struct LlmCallConfig {
    pub provider: LlmProvider,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub retries: u32,
}

impl Default for LlmCallConfig {
    fn default() -> Self {
        Self {
            provider: LlmProvider::OpenAiCompatible,
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key_env: None,
            max_tokens: 1024,
            timeout_secs: 30,
            retries: 2,
        }
    }
}

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
    provider: &LlmProvider,
    base_url: &str,
    provider_config: Option<&Path>,
) -> Result<Box<dyn LlmAdapter>> {
    validate_base_url(base_url)?;
    match provider {
        LlmProvider::OpenAiCompatible => Ok(Box::new(openai::OpenAiAdapter::new(base_url))),
        LlmProvider::Anthropic => Ok(Box::new(anthropic::AnthropicAdapter::new(base_url))),
        LlmProvider::DeepSeek => Ok(Box::new(openai::OpenAiAdapter::new("https://api.deepseek.com"))),
        LlmProvider::Gemini => Ok(Box::new(openai::OpenAiAdapter::new(
            "https://generativelanguage.googleapis.com/v1beta/openai",
        ))),
        LlmProvider::HttpJson => {
            let config_path = provider_config.ok_or_else(|| {
                anyhow!("--llm-provider http-json requires --llm-provider-config <path>")
            })?;
            Ok(Box::new(http_json::HttpJsonAdapter::load(config_path)?))
        }
    }
}

/// Require HTTPS for remote URLs; allow HTTP for localhost and loopback only.
pub fn validate_base_url(url: &str) -> Result<()> {
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

/// Split `text` into chunks of at most `max_chars` characters, breaking at line boundaries.
/// Lines that individually exceed `max_chars` are hard-cut at UTF-8 char boundaries.
/// Empty lines are skipped.
pub fn split_text_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if line.len() > max_chars {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            let mut start = 0;
            while start < line.len() {
                let mut end = (start + max_chars).min(line.len());
                while end > start && !line.is_char_boundary(end) {
                    end -= 1;
                }
                chunks.push(line[start..end].to_string());
                start = end;
            }
            continue;
        }
        if !current.is_empty() && current.len() + 1 + line.len() > max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() && !text.is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}
