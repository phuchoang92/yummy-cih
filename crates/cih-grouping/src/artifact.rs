use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::entry::FeatureGroupEntry;

/// Directory for feature artifacts belonging to a specific source graph version.
/// Path: `<repo>/.cih/artifacts-features/<graph_ver>/`
pub fn feature_artifact_dir(repo: &Path, graph_ver: &str) -> PathBuf {
    repo.join(".cih").join("artifacts-features").join(graph_ver)
}

/// Write raw strategy output (`groups-<strategy>.jsonl`) and merged canonical
/// output (`groups.jsonl`) into `dir`.
pub fn write_feature_artifacts(
    dir: &Path,
    strategy_name: &str,
    raw_entries: &[FeatureGroupEntry],
    merged_entries: &[FeatureGroupEntry],
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let raw_path = dir.join(format!("groups-{}.jsonl", strategy_name));
    std::fs::write(&raw_path, entries_to_jsonl(raw_entries)?)
        .with_context(|| format!("failed to write {}", raw_path.display()))?;

    let merged_path = dir.join("groups.jsonl");
    std::fs::write(&merged_path, entries_to_jsonl(merged_entries)?)
        .with_context(|| format!("failed to write {}", merged_path.display()))?;

    Ok(())
}

/// Read `groups.jsonl` from `dir`. Returns `Err` if the file is missing or malformed.
pub fn read_feature_artifact(dir: &Path) -> Result<Vec<FeatureGroupEntry>> {
    let path = dir.join("groups.jsonl");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse_jsonl(&content)
}

/// Return the path to the feature artifact dir if `groups.jsonl` exists for `graph_ver`.
pub fn find_feature_artifact_dir(repo: &Path, graph_ver: &str) -> Option<PathBuf> {
    let dir = feature_artifact_dir(repo, graph_ver);
    if dir.join("groups.jsonl").is_file() {
        Some(dir)
    } else {
        None
    }
}

/// Remove all subdirectories under `parent` except the one matching `keep_ver`.
/// Mirrors the `prune_other_versions` pattern used for other artifact families.
pub fn prune_feature_artifacts(parent: &Path, keep_ver: &str) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && entry.file_name().to_string_lossy() != keep_ver {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("failed to prune {}", path.display()))?;
        }
    }
    Ok(())
}

fn entries_to_jsonl(entries: &[FeatureGroupEntry]) -> Result<String> {
    let mut out = String::new();
    for e in entries {
        out.push_str(&serde_json::to_string(e)?);
        out.push('\n');
    }
    Ok(out)
}

fn parse_jsonl(content: &str) -> Result<Vec<FeatureGroupEntry>> {
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: FeatureGroupEntry = serde_json::from_str(line)
            .with_context(|| format!("failed to parse line {}", i + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
#[path = "artifact_tests.rs"]
mod tests;
