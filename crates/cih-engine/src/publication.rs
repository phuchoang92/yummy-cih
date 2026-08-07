//! One authoritative engine publication coordinator.
//!
//! The coordinator durably writes content metadata, loads a fresh immutable
//! physical graph, validates that load, and only then changes the backend-local
//! pointer with a fenced CAS. Registry persistence is intentionally a caller
//! step after this function returns `PublishedGraph`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use cih_core::{ensure_repository_id, graph_content_version, GraphArtifacts, RepositoryId};
use cih_graph_store::publication::{
    ArtifactVersion, CurrentPublication, GraphContentVersion, GraphPublicationEpoch,
    ManifestDigest, PublicationCasResult, ValidationDigest,
};
use cih_graph_store::LoadStats;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComponentManifest {
    kind: String,
    version: String,
    nodes_digest: String,
    edges_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GraphContentManifest {
    schema_version: u8,
    repository_id: RepositoryId,
    artifact_version: String,
    graph_content_version: String,
    merge_policy_version: u8,
    components: Vec<ComponentManifest>,
    required_indexes: Vec<String>,
}

#[derive(Serialize)]
struct ValidationReport<'a> {
    schema_version: u8,
    repository_id: &'a RepositoryId,
    physical_graph_key: &'a str,
    graph_content_version: &'a str,
    loaded_nodes: u64,
    loaded_edges: u64,
    indexes_built_by_loader: bool,
}

pub(crate) struct PublishedGraph {
    pub record: CurrentPublication,
    pub stats: LoadStats,
}

pub(crate) fn publish_complete_graph(
    repo_root: &Path,
    backend: &str,
    url: &str,
    base: &GraphArtifacts,
    overlays: &[(&str, &GraphArtifacts)],
) -> Result<PublishedGraph> {
    let repository_id = repository_identity(repo_root)?;
    let store = cih_store_factory::connect_publication_store(backend, url)?;
    let current = crate::runtime::block_on(store.current(&repository_id))
        .map_err(|error| anyhow::anyhow!("read current publication: {error}"))?;
    let expected_epoch = current.as_ref().map(|record| record.epoch.clone());
    let fencing_token = crate::runtime::block_on(store.allocate_fencing_token(&repository_id))
        .map_err(|error| anyhow::anyhow!("allocate publisher fencing token: {error}"))?;

    let overlay_versions = overlays
        .iter()
        .map(|(kind, artifact)| (*kind, artifact.version.as_str()))
        .collect::<Vec<_>>();
    // Analyzer artifact directories still use a legacy shortened content hash.
    // Promote that value to the publication schema's full digest identity
    // without changing the on-disk artifact/cache layout.
    let artifact_version = graph_content_version(base.version.as_str(), &[]);
    let content_version = graph_content_version(base.version.as_str(), &overlay_versions);
    let manifest = build_manifest(&repository_id, base, overlays, content_version.clone())?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = blake3::hash(&manifest_bytes).to_hex().to_string();
    write_immutable_metadata(repo_root, "manifests", &manifest_digest, &manifest_bytes)?;

    let epoch = GraphPublicationEpoch::allocate();
    let physical_graph_key = format!("repo-{}-epoch-{}", repository_id.as_str(), epoch.as_str());
    let overlay_artifacts = overlays
        .iter()
        .map(|(_, artifact)| *artifact)
        .collect::<Vec<_>>();
    let stats =
        crate::db::load_replacement(backend, url, &physical_graph_key, base, &overlay_artifacts)?;

    let validation = ValidationReport {
        schema_version: 1,
        repository_id: &repository_id,
        physical_graph_key: &physical_graph_key,
        graph_content_version: &content_version,
        loaded_nodes: stats.nodes,
        loaded_edges: stats.edges,
        indexes_built_by_loader: true,
    };
    let validation_bytes = serde_json::to_vec(&validation)?;
    let validation_digest = blake3::hash(&validation_bytes).to_hex().to_string();
    write_immutable_metadata(
        repo_root,
        "validations",
        &validation_digest,
        &validation_bytes,
    )?;

    let record = CurrentPublication {
        repository_id: repository_id.clone(),
        epoch,
        graph_content_version: GraphContentVersion::parse(content_version)?,
        physical_graph_key,
        artifact_version: ArtifactVersion::parse(artifact_version)?,
        graph_content_manifest_digest: ManifestDigest::parse(manifest_digest)?,
        validation_digest: ValidationDigest::parse(validation_digest)?,
        previous_epoch: expected_epoch.clone(),
    };
    match crate::runtime::block_on(store.compare_and_swap(
        &repository_id,
        expected_epoch.as_ref(),
        &record,
        fencing_token,
    ))? {
        PublicationCasResult::Published => Ok(PublishedGraph { record, stats }),
        PublicationCasResult::Conflict { current_epoch } => anyhow::bail!(
            "publication lost a concurrent CAS race; current epoch is {}",
            current_epoch
                .as_ref()
                .map_or("absent", |epoch| epoch.as_str())
        ),
        PublicationCasResult::StaleFencingToken { current_token } => anyhow::bail!(
            "publication fencing token was superseded by token {}",
            current_token.get()
        ),
    }
}

fn repository_identity(repo_root: &Path) -> Result<RepositoryId> {
    let canonical = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let rendered = canonical.to_string_lossy();
    let registry = cih_core::Registry::load();
    let preferred = registry
        .find(&rendered)
        .and_then(|entry| entry.repository_id.as_ref());
    ensure_repository_id(&canonical, preferred)
}

fn build_manifest(
    repository_id: &RepositoryId,
    base: &GraphArtifacts,
    overlays: &[(&str, &GraphArtifacts)],
    graph_content_version: String,
) -> Result<GraphContentManifest> {
    let mut components = Vec::with_capacity(overlays.len() + 1);
    components.push(component("base", base)?);
    for (kind, artifact) in overlays {
        components.push(component(kind, artifact)?);
    }
    Ok(GraphContentManifest {
        schema_version: 1,
        repository_id: repository_id.clone(),
        artifact_version: base.version.as_str().to_string(),
        graph_content_version,
        merge_policy_version: 1,
        components,
        required_indexes: vec![
            "Symbol.id".into(),
            "Symbol.kind".into(),
            "Symbol.name".into(),
            "Symbol.file".into(),
        ],
    })
}

fn component(kind: &str, artifact: &GraphArtifacts) -> Result<ComponentManifest> {
    Ok(ComponentManifest {
        kind: kind.to_string(),
        version: artifact.version.as_str().to_string(),
        nodes_digest: digest_file(&artifact.nodes_path)?,
        edges_digest: digest_file(&artifact.edges_path)?,
    })
}

fn digest_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read publication component {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn write_immutable_metadata(
    repo_root: &Path,
    kind: &str,
    digest: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    let directory = repo_root.join(".cih/publications").join(kind);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{digest}.json"));
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&path)? != bytes {
                anyhow::bail!(
                    "publication metadata digest collision at {}",
                    path.display()
                );
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

#[cfg(all(test, feature = "ladybug"))]
mod tests {
    use super::*;
    use cih_core::{Node, NodeId, NodeKind, Range, VersionId};

    fn artifact(root: &Path, version_byte: char, node_id: &str) -> GraphArtifacts {
        let version = std::iter::repeat_n(version_byte, 64).collect::<String>();
        GraphArtifacts::write(
            &root.join(&version),
            VersionId::new(version),
            &[Node {
                id: NodeId::new(node_id),
                kind: NodeKind::Method,
                name: node_id.into(),
                qualified_name: None,
                file: "Fixture.java".into(),
                range: Range::default(),
                props: None,
            }],
            &[],
        )
        .unwrap()
    }

    #[test]
    fn coordinator_publishes_immutable_graph_then_rotates_authoritative_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let graph_root = temp.path().join("graphs");
        let graph_url = graph_root.to_string_lossy().into_owned();
        let first_artifact = artifact(&repo.join(".cih/artifacts"), 'a', "Method:A#run/0");

        let first =
            publish_complete_graph(&repo, "ladybug", &graph_url, &first_artifact, &[]).unwrap();
        assert!(graph_root.join(&first.record.physical_graph_key).exists());
        assert!(repo
            .join(".cih/publications/manifests")
            .join(format!(
                "{}.json",
                first.record.graph_content_manifest_digest
            ))
            .exists());

        let second_artifact = artifact(&repo.join(".cih/artifacts"), 'b', "Method:B#run/0");
        let second =
            publish_complete_graph(&repo, "ladybug", &graph_url, &second_artifact, &[]).unwrap();
        assert_eq!(
            second.record.previous_epoch.as_ref(),
            Some(&first.record.epoch)
        );
        assert_ne!(
            second.record.physical_graph_key,
            first.record.physical_graph_key
        );
        assert!(graph_root.join(&first.record.physical_graph_key).exists());
        assert!(graph_root.join(&second.record.physical_graph_key).exists());

        let store = cih_store_factory::connect_publication_store("ladybug", &graph_url).unwrap();
        let current = crate::runtime::block_on(store.current(&second.record.repository_id))
            .unwrap()
            .unwrap();
        assert_eq!(current, second.record);
    }

    #[test]
    fn coordinator_promotes_legacy_artifact_versions_to_publication_digests() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let graph_root = temp.path().join("graphs");
        let graph_url = graph_root.to_string_lossy().into_owned();
        let mut legacy_artifact = artifact(&repo.join(".cih/artifacts"), 'a', "Method:A#run/0");
        let legacy_version = "a".repeat(16);
        legacy_artifact.version = VersionId::new(legacy_version.clone());

        let published =
            publish_complete_graph(&repo, "ladybug", &graph_url, &legacy_artifact, &[]).unwrap();

        assert_eq!(
            published.record.artifact_version.as_str(),
            graph_content_version(&legacy_version, &[])
        );
    }
}
