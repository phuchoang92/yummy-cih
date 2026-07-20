//! Graph-store construction and startup schema policy.

use std::sync::Arc;

use anyhow::Result;
use cih_graph_store::GraphStore;

use crate::config::{store_options, Config};

pub(crate) async fn build_store(cfg: &Config) -> Result<Arc<dyn GraphStore>> {
    let store = cih_store_factory::connect_store(
        &cfg.backend,
        &cfg.falkor_url,
        &cfg.graph_key,
        &store_options(cfg),
    )?;
    let mut last_err = None;
    for attempt in 1u32..=5 {
        match store.ensure_schema().await {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(error) => {
                tracing::warn!(
                    attempt,
                    error = %error,
                    "graph store not ready, retrying in 2s"
                );
                last_err = Some(error);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    if let Some(error) = last_err {
        return Err(anyhow::anyhow!("graph schema init failed: {error}"));
    }
    Ok(store)
}
