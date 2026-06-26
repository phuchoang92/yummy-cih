use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::versioning::latest_graph_artifacts;

pub(crate) fn run_embed(
    repo: PathBuf,
    pg_url: Option<String>,
    model: String,
    json: bool,
) -> Result<()> {
    let source = latest_graph_artifacts(&repo)?;
    let nodes = source
        .read_nodes()
        .with_context(|| format!("failed to read {}", source.nodes_path.display()))?;
    let model_kind = cih_embed::EmbedModelKind::parse(&model)?;
    let pg_url = pg_url
        .or_else(|| std::env::var("CIH_PG_URL").ok())
        .context("missing Postgres URL: pass --pg-url or set CIH_PG_URL")?;

    let embed = crate::runtime::block_on(async {
        let store = cih_embed::EmbedStore::connect(&pg_url, model_kind).await?;
        store.ensure_schema().await?;
        store.embed_nodes(&nodes, &repo).await
    })?;

    let artifacts_path = source
        .nodes_path
        .parent()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| source.nodes_path.display().to_string());
    let summary = EmbedCommandSummary {
        source_version: &source.version.0,
        artifacts_path,
        model: model_kind.label(),
        nodes_read: nodes.len(),
        embed,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        summary.print_human();
    }

    Ok(())
}

#[derive(Serialize)]
struct EmbedCommandSummary<'a> {
    source_version: &'a str,
    artifacts_path: String,
    model: &'a str,
    nodes_read: usize,
    embed: cih_embed::EmbedSummary,
}

impl EmbedCommandSummary<'_> {
    fn print_human(&self) {
        println!(
            "Embed: source graph {} -> model {}.",
            self.source_version, self.model
        );
        println!("Artifacts: {}", self.artifacts_path);
        println!(
            "Nodes: {} read, {} embeddable.",
            self.nodes_read, self.embed.nodes_considered
        );
        println!(
            "Chunks: {} total, {} embedded, {} skipped unchanged.",
            self.embed.chunks_total, self.embed.chunks_embedded, self.embed.chunks_skipped
        );
    }
}
