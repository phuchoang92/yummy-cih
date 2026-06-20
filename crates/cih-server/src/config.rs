//! Runtime config + the GraphStore factory (ports & adapters wiring).
//!
//! `CIH_GRAPH_BACKEND` selects the adapter — `falkor` now (dev / open-source),
//! `neptune` at go-live, `postgres` as the ~$0 fallback. Swapping backends is a
//! one-line env change; nothing else in the server cares which store it is.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cih_graph_store::GraphStore;

#[derive(Clone)]
pub struct Config {
    pub backend: String,
    pub bind: String,
    pub falkor_url: String,
    pub graph_key: String,
    pub artifacts_dir: Option<PathBuf>,
    pub pg_url: Option<String>,
    /// Agent LLM: OpenAI-compatible base URL (default: Gemini compat endpoint).
    pub agent_llm_base_url: String,
    /// Agent LLM: model name.
    pub agent_llm_model: String,
    /// Agent LLM: API key env var (default: auto-resolved from GEMINI/OPENAI/ANTHROPIC_API_KEY).
    pub agent_api_key: Option<String>,
    /// Optional static bearer token to protect /mcp and /graph. Unset = open (dev mode).
    pub api_token: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("backend", &self.backend)
            .field("bind", &self.bind)
            .field("falkor_url", &self.falkor_url)
            .field("graph_key", &self.graph_key)
            .field("artifacts_dir", &self.artifacts_dir)
            .field("pg_url", &self.pg_url.as_deref().map(|_| "[set]"))
            .field("agent_llm_base_url", &self.agent_llm_base_url)
            .field("agent_llm_model", &self.agent_llm_model)
            .field(
                "agent_api_key",
                &self.agent_api_key.as_deref().map(|_| "[REDACTED]"),
            )
            .field(
                "api_token",
                &self.api_token.as_deref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            backend: env("CIH_GRAPH_BACKEND", "falkor"),
            bind: env("CIH_BIND", "127.0.0.1:8080"),
            falkor_url: env("FALKOR_URL", "redis://127.0.0.1:6379"),
            graph_key: env("CIH_GRAPH_KEY", "cih"),
            artifacts_dir: std::env::var("CIH_ARTIFACTS_DIR").ok().map(PathBuf::from),
            pg_url: std::env::var("CIH_PG_URL").ok(),
            agent_llm_base_url: env(
                "CIH_AGENT_LLM_BASE_URL",
                "https://generativelanguage.googleapis.com/v1beta/openai",
            ),
            agent_llm_model: env("CIH_AGENT_LLM_MODEL", "gemini-2.0-flash"),
            agent_api_key: std::env::var("CIH_AGENT_API_KEY")
                .or_else(|_| std::env::var("GEMINI_API_KEY"))
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
                .ok(),
            api_token: std::env::var("CIH_API_TOKEN").ok(),
        }
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Build the configured `GraphStore`. This is the single place adapters are
/// named — the rest of the engine/MCP layer depends only on `dyn GraphStore`.
pub async fn build_store(cfg: &Config) -> Result<Arc<dyn GraphStore>> {
    match cfg.backend.as_str() {
        "falkor" => {
            let store = cih_falkor::FalkorStore::connect(&cfg.falkor_url, &cfg.graph_key)?;
            let mut last_err = None;
            for attempt in 1u32..=5 {
                match store.ensure_schema().await {
                    Ok(_) => { last_err = None; break; }
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "FalkorDB not ready, retrying in 2s");
                        last_err = Some(e);
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
            if let Some(e) = last_err {
                return Err(anyhow::anyhow!("FalkorDB schema init failed: {e}"));
            }
            Ok(Arc::new(store))
        }
        "neptune" => Err(anyhow!(
            "neptune adapter not implemented yet — go-live target (cih-neptune)"
        )),
        "postgres" => Err(anyhow!(
            "postgres-cte adapter not implemented yet — ~$0 fallback (cih-postgres)"
        )),
        other => Err(anyhow!(
            "unknown CIH_GRAPH_BACKEND='{other}' (use falkor|neptune|postgres)"
        )),
    }
}
