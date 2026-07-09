//! `cih-engine status` — registry entry, staleness, and feature status for one repo.

use anyhow::Result;
use cih_core::Registry;

use super::features::load_feature_status;

pub fn run(name: String, json: bool) -> Result<()> {
    let reg = Registry::load();
    if let Some(entry) = reg.find(&name) {
        let stale = reg.is_stale(&name);
        let repo_path = std::path::Path::new(&entry.path);
        let feat_status = load_feature_status(repo_path);
        if json {
            #[derive(serde::Serialize)]
            struct FeatureInfo {
                feature_count: usize,
                node_count: usize,
                pinned_count: usize,
                strategy: String,
                graph_version: String,
            }
            #[derive(serde::Serialize)]
            struct StatusOutput<'a> {
                entry: &'a cih_core::RegistryEntry,
                stale: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                features: Option<FeatureInfo>,
            }
            let features = feat_status.map(|fs| FeatureInfo {
                feature_count: fs.feature_count,
                node_count: fs.node_count,
                pinned_count: fs.pinned_count,
                strategy: fs.strategy,
                graph_version: fs.graph_version,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&StatusOutput {
                    entry,
                    stale,
                    features
                })?
            );
        } else {
            println!("name:          {}", entry.name);
            println!("path:          {}", entry.path);
            println!("graph_key:     {}", entry.graph_key);
            println!("indexed_at:    {}", entry.indexed_at);
            println!(
                "git_head:      {}",
                entry.last_git_head.as_deref().unwrap_or("(unknown)")
            );
            println!("stale:         {}", stale);
            println!("nodes:         {}", entry.stats.nodes);
            println!("edges:         {}", entry.stats.edges);
            println!("files:         {}", entry.stats.files);
            println!("routes:        {}", entry.stats.routes);
            println!("communities:   {}", entry.stats.communities);
            println!("processes:     {}", entry.stats.processes);
            if let Some(fs) = feat_status {
                println!(
                    "features:      {} ({} nodes, strategy: {})",
                    fs.feature_count, fs.node_count, fs.strategy
                );
                println!("pinned:        {}", fs.pinned_count);
                println!(
                    "feat_version:  {}",
                    &fs.graph_version[..fs.graph_version.len().min(16)]
                );
            }
        }
    } else {
        eprintln!("Registry entry not found for '{name}'. Run `cih-engine analyze <repo>` first.");
        std::process::exit(1);
    }
    Ok(())
}
