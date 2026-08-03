//! `cih-engine artifact` — export/import/bootstrap CIH bundle archives.

use anyhow::{Context, Result};

use crate::DEFAULT_GRAPH_KEY;

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
            backend,
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

            // Publish through the same owned staging path as analyze/discover.
            // Loading directly into the live key can expose a partial base-only
            // graph when the optional community load fails.
            let backend = backend.unwrap_or_else(|| crate::DEFAULT_BACKEND.to_string());
            let falkor_url = falkor_url.unwrap_or_else(|| crate::default_db_url(&backend));
            let graph_key = graph_key.unwrap_or_else(|| DEFAULT_GRAPH_KEY.to_string());
            let overlay_components: Vec<(&str, &GraphArtifacts)> = community
                .iter()
                .map(|artifact| ("community", artifact))
                .collect();
            let published = crate::publication::publish_complete_graph(
                &repo,
                &backend,
                &falkor_url,
                &artifacts,
                &overlay_components,
            )?;
            tracing::info!(
                nodes = published.stats.nodes,
                edges = published.stats.edges,
                backend,
                graph = published.record.physical_graph_key,
                "bootstrap graph publication complete"
            );

            // Register in registry.
            let root_abs = repo.canonicalize().unwrap_or(repo.clone());
            let registry_path = cih_core::RegistryStore::global()?.path().to_path_buf();
            let overlays: Vec<&GraphArtifacts> = community.iter().collect();
            register_repo_in_registry(
                &registry_path,
                &root_abs,
                &artifacts,
                &overlays,
                &graph_key,
                &published.record,
            )
            .with_context(|| {
                format!(
                    "graph publication succeeded, but registry promotion at {} failed; \
                         the bootstrap is incomplete",
                    registry_path.display()
                )
            })?;

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

fn register_repo_in_registry(
    registry_path: &std::path::Path,
    root: &std::path::Path,
    artifacts: &cih_core::GraphArtifacts,
    overlays: &[&cih_core::GraphArtifacts],
    graph_key: &str,
    publication: &cih_graph_store::publication::CurrentPublication,
) -> Result<()> {
    use cih_core::{ensure_repository_id, RegistryEntry, RegistryStats, RegistryStore};

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
    let community_artifacts_dir = overlays.first().and_then(|overlay| {
        overlay
            .nodes_path
            .parent()
            .map(|path| path.to_string_lossy().to_string())
    });
    let artifact_version = artifacts.version.as_str().to_string();
    let mut entry = RegistryEntry {
        repository_id: None,
        name: name.clone(),
        path: root_str.clone(),
        graph_key: graph_key.to_string(),
        artifacts_dir,
        latest_artifact_version: Some(artifact_version.clone()),
        published_artifact_version: Some(publication.artifact_version.to_string()),
        published_graph_content_version: Some(publication.graph_content_version.to_string()),
        published_epoch: Some(publication.epoch.to_string()),
        community_artifacts_dir,
        indexed_at: cih_core::registry::now_rfc3339(),
        last_git_head: None,
        // Placeholder entry — real counts land on the next analyze.
        stats: RegistryStats::default(),
    };
    let identity_root = root.to_path_buf();
    let publication = publication.clone();
    RegistryStore::new(registry_path).update(move |registry| {
        let preferred = registry
            .find(&root_str)
            .and_then(|registered| registered.repository_id.as_ref());
        let repository_id = ensure_repository_id(&identity_root, preferred)?;
        if repository_id != publication.repository_id {
            anyhow::bail!("authoritative publication repository identity does not match registry");
        }
        entry.repository_id = Some(repository_id);
        registry.upsert(entry);
        Ok(())
    })?;
    println!("Registered repo '{}' in registry.", name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{GraphArtifacts, RegistryStore, VersionId};
    use cih_graph_store::publication::{
        ArtifactVersion, CurrentPublication, GraphContentVersion, GraphPublicationEpoch,
        ManifestDigest, ValidationDigest,
    };

    fn artifacts(path: std::path::PathBuf, version: &str) -> GraphArtifacts {
        GraphArtifacts {
            nodes_path: path.join("nodes.jsonl"),
            edges_path: path.join("edges.jsonl"),
            version: VersionId::new(version),
        }
    }

    #[test]
    fn bootstrap_registry_mirror_retains_imported_overlay_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo root");
        let base = artifacts(repo.join(".cih/artifacts/base-v1"), "base-v1");
        let community = artifacts(
            repo.join(".cih/artifacts-community/community-v2"),
            "community-v2",
        );
        let registry_path = temp.path().join("registry.json");
        let repository_id = cih_core::ensure_repository_id(&repo, None).unwrap();
        let publication = CurrentPublication {
            repository_id,
            epoch: GraphPublicationEpoch::parse("1".repeat(64)).unwrap(),
            graph_content_version: GraphContentVersion::parse("2".repeat(64)).unwrap(),
            physical_graph_key: "physical-fixture".into(),
            artifact_version: ArtifactVersion::parse("3".repeat(64)).unwrap(),
            graph_content_manifest_digest: ManifestDigest::parse("4".repeat(64)).unwrap(),
            validation_digest: ValidationDigest::parse("5".repeat(64)).unwrap(),
            previous_epoch: None,
        };

        register_repo_in_registry(
            &registry_path,
            &repo,
            &base,
            &[&community],
            "fixture-graph",
            &publication,
        )
        .expect("registry promotion");

        let snapshot = RegistryStore::new(&registry_path).load().expect("registry");
        let entry = snapshot
            .registry
            .find(repo.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(
            entry.community_artifacts_dir.as_deref(),
            community
                .nodes_path
                .parent()
                .map(|path| path.to_string_lossy())
                .as_deref()
        );
        assert!(entry.repository_id.is_some());
        assert!(entry.published_epoch.is_some());
    }
}
