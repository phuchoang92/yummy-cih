//! Runtime config + the GraphStore factory (ports & adapters wiring).
//!
//! `CIH_GRAPH_BACKEND` selects the adapter — `falkor` now (dev / open-source),
//! `neptune` at go-live, `postgres` as the ~$0 fallback. Swapping backends is a
//! one-line env change; nothing else in the server cares which store it is.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cih_graph_store::GraphStore;

#[derive(Clone, Debug)]
pub struct Config {
    pub backend: String,
    pub bind: String,
    pub falkor_url: String,
    pub graph_key: String,
    pub artifacts_dir: Option<PathBuf>,
    pub pg_url: Option<String>,
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
            store.ensure_schema().await?;
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
