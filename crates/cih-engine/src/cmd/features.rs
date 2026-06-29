use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_grouping::{FeatureGroupEntry, FeatureOverrides};
use serde::Serialize;

// ── features show ─────────────────────────────────────────────────────────────

pub(crate) fn run_features_show(repo: PathBuf, json: bool) -> Result<()> {
    let (dir, entries) = load_feature_artifact(&repo)?;
    let version = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let summary = build_summary(&version, &entries);

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_summary(&repo, &summary);
    }
    Ok(())
}

fn load_feature_artifact(repo: &Path) -> Result<(PathBuf, Vec<FeatureGroupEntry>)> {
    let parent = repo.join(".cih").join("artifacts-features");
    let dir = latest_version_dir(&parent).with_context(|| {
        format!(
            "no feature artifacts at {} — run `discover` first",
            parent.display()
        )
    })?;
    let entries = cih_grouping::read_feature_artifact(&dir)
        .with_context(|| format!("failed to read groups.jsonl from {}", dir.display()))?;
    Ok((dir, entries))
}

fn latest_version_dir(parent: &Path) -> Result<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", parent.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.join("groups.jsonl").is_file())
        .collect();
    dirs.sort();
    dirs.pop()
        .ok_or_else(|| anyhow::anyhow!("no groups.jsonl found under {}", parent.display()))
}

#[derive(Serialize)]
struct FeatureRow {
    name: String,
    node_count: usize,
    /// Most common strategy in this feature's assignments.
    strategy: String,
    pinned_count: usize,
}

#[derive(Serialize)]
struct FeatureSummary {
    graph_version: String,
    features: Vec<FeatureRow>,
    totals: Totals,
}

#[derive(Serialize)]
struct Totals {
    features: usize,
    nodes: usize,
    pinned: usize,
}

fn build_summary(version: &str, entries: &[FeatureGroupEntry]) -> FeatureSummary {
    // Group by feature name.
    let mut by_feature: HashMap<&str, Vec<&FeatureGroupEntry>> = HashMap::new();
    for e in entries {
        by_feature.entry(e.name.as_str()).or_default().push(e);
    }

    let mut rows: Vec<FeatureRow> = by_feature
        .iter()
        .map(|(name, group)| {
            let pinned_count = group.iter().filter(|e| e.pinned).count();
            // Pick the most common non-override strategy as display strategy.
            let strategy = dominant_strategy(group);
            FeatureRow {
                name: name.to_string(),
                node_count: group.len(),
                strategy,
                pinned_count,
            }
        })
        .collect();

    rows.sort_by(|a, b| b.node_count.cmp(&a.node_count).then(a.name.cmp(&b.name)));

    let total_nodes = entries.len();
    let total_pinned = entries.iter().filter(|e| e.pinned).count();
    let feature_count = rows.len();

    FeatureSummary {
        graph_version: version.to_string(),
        features: rows,
        totals: Totals {
            features: feature_count,
            nodes: total_nodes,
            pinned: total_pinned,
        },
    }
}

fn dominant_strategy(group: &[&FeatureGroupEntry]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in group {
        if e.strategy != "override" {
            *counts.entry(e.strategy.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| "override".to_string())
}

fn print_summary(repo: &Path, summary: &FeatureSummary) {
    let repo_name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let ver = &summary.graph_version[..summary.graph_version.len().min(8)];
    crate::ui::print_header("Features", repo_name, Some(ver));

    // Column widths.
    let name_w = summary
        .features
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let strategy_w = 10usize;

    eprintln!(
        "     {:<name_w$}  {:>6}  {:<strategy_w$}  {}",
        "Feature",
        "Nodes",
        "Strategy",
        "Pinned",
        name_w = name_w,
        strategy_w = strategy_w
    );
    eprintln!(
        "     {}  {}  {}  {}",
        "─".repeat(name_w),
        "──────",
        "─".repeat(strategy_w),
        "──────"
    );

    for row in &summary.features {
        let pin = if row.pinned_count > 0 {
            format!("  \x1b[33m● {}\x1b[0m", row.pinned_count)
        } else {
            String::new()
        };
        eprintln!(
            "     {:<name_w$}  {:>6}  {:<strategy_w$}{}",
            row.name,
            row.node_count,
            row.strategy,
            pin,
            name_w = name_w,
            strategy_w = strategy_w
        );
    }

    eprintln!();
    eprintln!(
        "     \x1b[2m{} features  ·  {} nodes  ·  {} pinned\x1b[0m",
        summary.totals.features, summary.totals.nodes, summary.totals.pinned
    );
    eprintln!();
}

// ── features override ─────────────────────────────────────────────────────────

pub(crate) fn run_features_override(
    repo: PathBuf,
    node_id: String,
    feature: String,
    reason: String,
) -> Result<()> {
    let path = repo.join(".cih").join("feature-overrides.json");

    let mut overrides = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str::<FeatureOverrides>(&text)
            .with_context(|| format!("malformed {}", path.display()))?
    } else {
        FeatureOverrides::default()
    };

    // Upsert: update existing entry for this node_id, or append a new one.
    let existing = overrides.entries.iter_mut().find(|e| e.node_id == node_id);

    let is_update = existing.is_some();
    if let Some(entry) = existing {
        entry.feature = feature.clone();
        if !reason.is_empty() {
            entry.reason = reason.clone();
        }
    } else {
        overrides.entries.push(cih_grouping::FeatureOverrideEntry {
            node_id: node_id.clone(),
            feature: feature.clone(),
            reason: reason.clone(),
        });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&overrides)?;
    std::fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;

    let action = if is_update { "Updated" } else { "Added" };
    eprintln!("{action} override: {node_id} → \x1b[1m{feature}\x1b[0m");
    eprintln!("Written to {}", path.display());
    eprintln!("Re-run `discover` to apply.");
    Ok(())
}

// ── feature info for status command ───────────────────────────────────────────

pub(crate) struct FeatureStatus {
    pub graph_version: String,
    pub feature_count: usize,
    pub node_count: usize,
    pub pinned_count: usize,
    pub strategy: String,
}

pub(crate) fn load_feature_status(repo: &Path) -> Option<FeatureStatus> {
    let parent = repo.join(".cih").join("artifacts-features");
    let dir = latest_version_dir(&parent).ok()?;
    let version = dir.file_name()?.to_str()?.to_string();
    let entries = cih_grouping::read_feature_artifact(&dir).ok()?;

    let mut features = std::collections::HashSet::new();
    for e in &entries {
        features.insert(e.name.as_str());
    }
    let pinned = entries.iter().filter(|e| e.pinned).count();
    let strategy = dominant_strategy(&entries.iter().collect::<Vec<_>>());

    Some(FeatureStatus {
        graph_version: version,
        feature_count: features.len(),
        node_count: entries.len(),
        pinned_count: pinned,
        strategy,
    })
}
