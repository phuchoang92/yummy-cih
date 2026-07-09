use std::path::Path;

use anyhow::{Context, Result};
use cih_core::{Edge, GraphArtifacts, Node, ParsedFile};

/// blake3 (first 16 hex) over deterministic nodes+edges+IR → graph version.
pub fn content_version(nodes: &[Node], edges: &[Edge], parsed_files: &[ParsedFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    for node in nodes {
        hasher.update(&serde_json::to_vec(node).unwrap_or_default());
        hasher.update(b"\n");
    }
    for edge in edges {
        hasher.update(&serde_json::to_vec(edge).unwrap_or_default());
        hasher.update(b"\n");
    }
    for parsed in parsed_files {
        hasher.update(&serde_json::to_vec(parsed).unwrap_or_default());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex()[..16].to_string()
}

pub fn latest_graph_artifacts(repo: &Path) -> Result<GraphArtifacts> {
    let parent = repo.join(".cih").join("artifacts");
    GraphArtifacts::latest_in_dir(&parent).with_context(|| {
        format!(
            "run `analyze` first — no graph artifacts at {}",
            parent.display()
        )
    })
}

pub fn discover_version(nodes: &[Node], edges: &[Edge]) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut node_ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    node_ids.sort_unstable();
    for id in node_ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    let mut edge_keys: Vec<String> = edges
        .iter()
        .map(|e| {
            format!(
                "{}\t{}\t{}",
                e.src.as_str(),
                e.dst.as_str(),
                e.kind.cypher_label()
            )
        })
        .collect();
    edge_keys.sort_unstable();
    for key in edge_keys {
        hasher.update(key.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex()[..16].to_string()
}

/// Remove every direct child dir of `parent` except `keep`. Best-effort: failures to
/// remove a stale dir are logged, not fatal.
pub fn prune_other_versions(parent: &Path, keep: &str) -> Result<()> {
    if !parent.exists() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(parent).with_context(|| format!("failed to read {}", parent.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && entry.file_name().to_str() != Some(keep) {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), error = %err, "Failed to prune stale version dir");
            } else {
                tracing::debug!(path = %path.display(), "Pruned stale version dir");
            }
        }
    }
    Ok(())
}
