use anyhow::{Context, Result};
use cih_core::GraphArtifacts;
use cih_falkor::FalkorStore;
use cih_graph_store::{GraphStore, LoadStats};

/// Outcome of the FalkorDB load step — distinguishes a deliberate skip from a failure.
pub(crate) enum LoadOutcome {
    Loaded(LoadStats),
    Reused,
    Skipped,
    Failed(String),
}

impl LoadOutcome {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            LoadOutcome::Loaded(_) => "loaded",
            LoadOutcome::Reused => "reused",
            LoadOutcome::Skipped => "skipped",
            LoadOutcome::Failed(_) => "failed",
        }
    }

    pub(crate) fn stats(&self) -> Option<&LoadStats> {
        match self {
            LoadOutcome::Loaded(stats) => Some(stats),
            _ => None,
        }
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            LoadOutcome::Failed(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Run the async FalkorDB bulk_load inside a short-lived tokio runtime.
/// The engine CLI is otherwise synchronous (rayon for parse, blocking I/O for
/// scan), so we spin up a minimal runtime only for the DB call.
pub(crate) fn load_to_falkor(
    url: &str,
    graph_key: &str,
    artifacts: &GraphArtifacts,
) -> Result<cih_graph_store::LoadStats> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    rt.block_on(async {
        let staging_key = format!("{graph_key}-staging");
        let store = FalkorStore::connect(url, &staging_key)
            .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
        let _ = store.drop_graph().await;
        store
            .ensure_schema()
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB ensure_schema: {e}"))?;
        let stats = store
            .bulk_load(artifacts)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB bulk_load: {e}"))?;
        store
            .publish_to(graph_key)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB publish: {e}"))?;
        if let Err(err) = store.drop_graph().await {
            tracing::warn!(graph = staging_key, error = %err, "failed to drop FalkorDB staging graph");
        }
        Ok(stats)
    })
}
