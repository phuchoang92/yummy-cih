//! `cih-engine artifact` — export/import/bootstrap CIH bundle archives.

use anyhow::Result;

use crate::runtime;
use crate::{DEFAULT_FALKOR_URL, DEFAULT_GRAPH_KEY};

use super::args::ArtifactCommand;

pub fn run(command: ArtifactCommand) -> Result<()> {
    use cih_core::GraphArtifacts;
    match command {
        ArtifactCommand::Export { repo, out } => {
            let cih_dir = repo.join(".cih");
            let artifacts_dir = cih_dir.join("artifacts");
            // Find the latest version dir.
            let version_dir = find_latest_version_dir(&artifacts_dir)?;
            let version_id = version_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let artifacts = GraphArtifacts {
                nodes_path: version_dir.join("nodes.jsonl"),
                edges_path: version_dir.join("edges.jsonl"),
                version: cih_core::VersionId::new(version_id.clone()),
            };
            let bundle_path = out.unwrap_or_else(|| cih_dir.join("graph.db.zst"));
            let manifest = artifacts.export_bundle(
                None,
                &cih_dir.join("file-hashes.json"),
                &cih_dir.join("scope.json"),
                &cih_dir.join("repo-map.json"),
                &bundle_path,
            )?;
            println!(
                "Bundle exported to {}: {} files, version {}",
                bundle_path.display(),
                manifest.file_count,
                &manifest.artifact_version[..8.min(manifest.artifact_version.len())]
            );
            Ok(())
        }
        ArtifactCommand::Import { repo, bundle } => {
            let cih_dir = repo.join(".cih");
            let (_, _, manifest) = GraphArtifacts::import_bundle(&bundle, &cih_dir)?;
            println!(
                "Bundle imported: repo={}, {} files, version {}",
                manifest.repo_name,
                manifest.file_count,
                &manifest.artifact_version[..8.min(manifest.artifact_version.len())]
            );
            Ok(())
        }
        ArtifactCommand::Bootstrap {
            repo,
            bundle,
            falkor_url,
            graph_key,
        } => {
            let cih_dir = repo.join(".cih");
            let (artifacts, community, manifest) =
                GraphArtifacts::import_bundle(&bundle, &cih_dir)?;
            println!(
                "Bundle imported: {} files, version {}",
                manifest.file_count,
                &manifest.artifact_version[..8.min(manifest.artifact_version.len())]
            );

            // Bulk-load into FalkorDB.
            let falkor_url = falkor_url.unwrap_or_else(|| DEFAULT_FALKOR_URL.to_string());
            let graph_key = graph_key.unwrap_or_else(|| DEFAULT_GRAPH_KEY.to_string());
            runtime::block_on(async {
                use cih_falkor::FalkorStore;
                use cih_graph_store::GraphStore;
                let store = FalkorStore::connect(&falkor_url, &graph_key)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                store
                    .ensure_schema()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                store
                    .bulk_load(&artifacts)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if let Some(comm) = community {
                    store
                        .bulk_load(&comm)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                Ok::<(), anyhow::Error>(())
            })?;

            // Register in registry.
            let root_abs = repo.canonicalize().unwrap_or(repo.clone());
            let registry_path = dirs_next_or_home().join(".cih").join("registry.json");
            if let Err(e) =
                register_repo_in_registry(&registry_path, &root_abs, &artifacts, &graph_key)
            {
                tracing::warn!(
                    error = %e,
                    registry = %registry_path.display(),
                    "bootstrap loaded the graph but failed to register the repo; \
                     it will not appear in `list_repos` until re-registered"
                );
            }

            println!("Bootstrap complete. Graph key: {graph_key}");
            Ok(())
        }
    }
}

fn find_latest_version_dir(artifacts_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(artifacts_dir)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", artifacts_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no artifact versions found in {}", artifacts_dir.display()))
}

fn dirs_next_or_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn register_repo_in_registry(
    registry_path: &std::path::Path,
    root: &std::path::Path,
    artifacts: &cih_core::GraphArtifacts,
    graph_key: &str,
) -> Result<()> {
    use cih_core::{Registry, RegistryEntry, RegistryStats};
    let mut registry = if registry_path.exists() {
        let bytes = std::fs::read(registry_path)?;
        serde_json::from_slice::<Registry>(&bytes).unwrap_or_default()
    } else {
        Registry::default()
    };
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let root_str = root.to_string_lossy().to_string();
    let artifacts_dir = artifacts
        .nodes_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let entry = RegistryEntry {
        name: name.clone(),
        path: root_str.clone(),
        graph_key: graph_key.to_string(),
        artifacts_dir,
        community_artifacts_dir: None,
        indexed_at: cih_core::registry::now_rfc3339(),
        last_git_head: None,
        // Placeholder entry — real counts land on the next analyze.
        stats: RegistryStats::default(),
    };
    registry.entries.retain(|r| r.path != root_str);
    registry.entries.push(entry);
    if let Some(parent) = registry_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&registry).map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(registry_path, json)?;
    println!("Registered repo '{}' in registry.", name);
    Ok(())
}
