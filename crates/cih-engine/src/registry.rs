use std::path::Path;

use anyhow::Context as _;
use cih_core::{
    ensure_repository_id, git_head, graph_content_version, new_publication_epoch, now_rfc3339,
    GroupRegistry, Registry, RegistryEntry, RegistryStats,
};

use crate::analyze::EmitOutcome;
use crate::discover::DiscoverOutcome;

fn repo_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn registry_path_string(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    #[cfg(windows)]
    {
        // Analyze persists the canonical root through scan::normalize_path,
        // which uses forward slashes even for Windows verbatim paths. Keep
        // discover on the same registry representation while retaining the
        // native Path for filesystem and repository-identity operations.
        rendered.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        rendered.into_owned()
    }
}

pub(crate) fn entry_from_analyze(emit: &EmitOutcome, graph_key: &str) -> RegistryEntry {
    let path = emit.scope_file.repo_root.clone();
    RegistryEntry {
        repository_id: None,
        name: repo_name(&path),
        graph_key: graph_key.to_string(),
        artifacts_dir: emit.artifacts_dir.display().to_string(),
        latest_artifact_version: Some(emit.version.clone()),
        published_artifact_version: None,
        published_graph_content_version: None,
        published_epoch: None,
        community_artifacts_dir: None,
        indexed_at: now_rfc3339(),
        last_git_head: git_head(Path::new(&path)),
        stats: RegistryStats {
            nodes: emit.node_count,
            edges: emit.edge_count,
            files: emit.parsed_file_count,
            // Analyze owns the route stat: Route nodes are emitted here, so the
            // count must never wait for a `discover` run that MCP indexing never
            // performs (that was the `routes: 0` status bug). Discover still
            // overwrites it with its richer count when it does run.
            routes: emit.route_node_count,
            // A no-op reuse does not inspect Route nodes. Its persisted value
            // is carried from the prior registry entry below; without one it
            // must remain explicitly stale rather than claiming a current 0.
            routes_current: !emit.reused_artifacts,
            communities: 0,
            processes: 0,
            resolved_edges: emit.resolved_edge_count,
            unresolved_refs: emit.unresolved_reference_count,
            callable_coverage: crate::analyze::callable_coverage(
                emit.callable_node_count,
                emit.syntactic_callables,
            ),
            // This entry has not been published yet. `persist_analyze` binds
            // the in-memory report only after the graph load succeeds.
            published_graph_report: None,
        },
        path,
    }
}

pub(crate) fn update_entry_from_discover(entry: &mut RegistryEntry, disc: &DiscoverOutcome) {
    entry.community_artifacts_dir = Some(disc.artifacts_dir.display().to_string());
    entry.stats.routes = disc.route_count;
    entry.stats.routes_current = true;
    entry.stats.communities = disc.community_count;
    entry.stats.processes = disc.process_count;
    let base_version = disc.source_artifacts.version.as_str();
    entry.latest_artifact_version = Some(base_version.to_string());
    entry.published_artifact_version = Some(base_version.to_string());
    entry.published_graph_content_version = Some(graph_content_version(
        base_version,
        &[("community", disc.version.as_str())],
    ));
    entry.stats.published_graph_report = disc.published_graph_report.clone().filter(|report| {
        entry
            .published_graph_content_version
            .as_deref()
            .is_some_and(|version| report.matches_content(version))
    });
    entry.published_epoch = Some(new_publication_epoch());
}

/// Persist an `EmitOutcome` to the global registry. Returns whether the durable
/// registry transaction succeeded so callers can defer irreversible cleanup.
pub(crate) fn persist_analyze(emit: &EmitOutcome, graph_key: &str) -> anyhow::Result<()> {
    let mut entry = entry_from_analyze(emit, graph_key);
    let repo = entry.name.clone();
    let reused_artifacts = emit.reused_artifacts;
    let fresh_report = emit.published_graph_report.clone();
    let published_content_version = graph_content_version(&emit.version, &[]);
    let update = Registry::update(move |reg| {
        // A reused no-op run re-measures nothing and reports zeros for the
        // resolve/coverage fields. Carry the previous values forward instead of
        // overwriting a perfectly good index with zeros. This read must happen
        // inside the registry transaction so a concurrent discover/analyze
        // update cannot be lost between a separate load and save.
        let previous = reg.find(&entry.path).cloned();
        if reused_artifacts {
            if let Some(prev) = previous.as_ref() {
                entry.stats.resolved_edges = prev.stats.resolved_edges;
                entry.stats.unresolved_refs = prev.stats.unresolved_refs;
                entry.stats.callable_coverage = prev.stats.callable_coverage;
                entry.stats.routes = prev.stats.routes;
                entry.stats.routes_current = prev.stats.routes_current;
                entry.stats.published_graph_report = prev
                    .stats
                    .published_graph_report
                    .clone()
                    .filter(|report| report.matches_content(&published_content_version));
            }
        } else {
            entry.stats.published_graph_report =
                fresh_report.filter(|report| report.matches_content(&published_content_version));
        }
        let preferred_id = previous
            .as_ref()
            .and_then(|previous| previous.repository_id.as_ref());
        entry.repository_id = Some(ensure_repository_id(Path::new(&entry.path), preferred_id)?);
        entry.published_artifact_version = Some(emit.version.clone());
        entry.published_graph_content_version = Some(published_content_version);
        entry.published_epoch = Some(new_publication_epoch());
        reg.upsert(entry);
        Ok(())
    });

    let update = update.context("failed to update registry after analyze publication")?;
    crate::group_sync::auto_sync_groups_for_repo(
        &GroupRegistry::load(),
        &update.snapshot.registry,
        &repo,
    );
    Ok(())
}

/// Persist a `DiscoverOutcome` update. Returns whether a matching entry was
/// durably updated, allowing cleanup to remain behind registry promotion.
pub(crate) fn persist_discover(repo_path: &Path, disc: &DiscoverOutcome) -> anyhow::Result<()> {
    let path_str = registry_path_string(repo_path);
    let update = Registry::update(|reg| {
        let entry = reg.find_mut(&path_str).ok_or_else(|| {
            anyhow::anyhow!("registry entry not found for {path_str}; run analyze first")
        })?;
        entry.repository_id = Some(ensure_repository_id(
            repo_path,
            entry.repository_id.as_ref(),
        )?);
        update_entry_from_discover(entry, disc);
        Ok(entry.name.clone())
    });

    let update = update.context("failed to update registry after discover publication")?;
    crate::group_sync::auto_sync_groups_for_repo(
        &GroupRegistry::load(),
        &update.snapshot.registry,
        &update.value,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cih_core::{GraphArtifacts, RegistryGraphReport, RegistryStats, VersionId};

    use super::*;
    use crate::analyze::CacheStats;
    use crate::scope::{ScopeFile, ScopeRequest};

    fn graph_artifacts(root: &Path, version: &str) -> GraphArtifacts {
        let directory = root.join(version);
        GraphArtifacts {
            nodes_path: directory.join("nodes.jsonl"),
            edges_path: directory.join("edges.jsonl"),
            version: VersionId::new(version),
        }
    }

    fn registry_entry(repo: &Path) -> RegistryEntry {
        RegistryEntry {
            repository_id: None,
            name: "repo".into(),
            path: repo.display().to_string(),
            graph_key: "cih".into(),
            artifacts_dir: repo.join(".cih/artifacts/base-v1").display().to_string(),
            latest_artifact_version: Some("base-v1".into()),
            published_artifact_version: Some("base-v1".into()),
            published_graph_content_version: Some(graph_content_version("base-v1", &[])),
            published_epoch: Some(new_publication_epoch()),
            community_artifacts_dir: None,
            indexed_at: "2026-01-01T00:00:00Z".into(),
            last_git_head: None,
            stats: RegistryStats::default(),
        }
    }

    #[cfg(windows)]
    #[test]
    fn discover_registry_path_matches_analyze_windows_format() {
        assert_eq!(
            registry_path_string(Path::new(r"\\?\D:\CIH Home 数据\fixture repo 日本語")),
            "//?/D:/CIH Home 数据/fixture repo 日本語"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn discover_registry_path_preserves_unix_backslash_names() {
        assert_eq!(
            registry_path_string(Path::new(r"/tmp/repo\literal")),
            r"/tmp/repo\literal"
        );
    }

    #[test]
    fn analyze_entry_records_latest_without_claiming_publication() {
        let repo = tempfile::tempdir().unwrap();
        let artifacts = graph_artifacts(&repo.path().join(".cih/artifacts"), "base-v2");
        let emit = EmitOutcome {
            scope_file: ScopeFile {
                repo_root: repo.path().display().to_string(),
                version: "scope-v1".into(),
                selection: ScopeRequest::default(),
                modules: Vec::new(),
                file_count: 0,
                files: Vec::new(),
            },
            scope_path: PathBuf::new(),
            artifacts,
            parsed_files_path: PathBuf::new(),
            artifacts_dir: repo.path().join(".cih/artifacts/base-v2"),
            version: "base-v2".into(),
            node_count: 1,
            edge_count: 0,
            resolved_edge_count: 0,
            jar_node_count: 0,
            jar_failed: 0,
            unresolved_reference_count: 0,
            unresolved_external_fqcns: Vec::new(),
            parsed_file_count: 1,
            skipped_count: 0,
            reused_artifacts: false,
            cache_stats: CacheStats::default(),
            syntactic_callables: 0,
            callable_node_count: 0,
            route_node_count: 0,
            published_graph_report: None,
        };

        let entry = entry_from_analyze(&emit, "cih");
        assert_eq!(entry.latest_artifact_version.as_deref(), Some("base-v2"));
        assert!(entry.published_artifact_version.is_none());
        assert!(entry.published_graph_content_version.is_none());
        assert!(entry.published_epoch.is_none());
    }

    #[test]
    fn discover_promotion_binds_base_and_overlay_and_rotates_epoch() {
        let repo = tempfile::tempdir().unwrap();
        let mut entry = registry_entry(repo.path());
        let previous_epoch = entry.published_epoch.clone();
        let outcome = DiscoverOutcome {
            source_artifacts: graph_artifacts(&repo.path().join("base"), "base-v2"),
            artifacts: graph_artifacts(&repo.path().join("community"), "community-v3"),
            artifacts_dir: repo.path().join("community/community-v3"),
            version: "community-v3".into(),
            route_count: 2,
            community_count: 3,
            process_count: 4,
            member_edge_count: 5,
            step_edge_count: 6,
            node_count: 7,
            edge_count: 8,
            feature_count: 9,
            published_graph_report: Some(RegistryGraphReport {
                schema_version: 1,
                graph_content_version: graph_content_version(
                    "base-v2",
                    &[("community", "community-v3")],
                ),
                total_nodes: 0,
                total_edges: 0,
                kinds: Vec::new(),
                symbol_hubs: Vec::new(),
            }),
        };

        update_entry_from_discover(&mut entry, &outcome);

        assert_eq!(entry.latest_artifact_version.as_deref(), Some("base-v2"));
        assert_eq!(entry.published_artifact_version.as_deref(), Some("base-v2"));
        assert_eq!(
            entry.published_graph_content_version.as_deref(),
            Some(graph_content_version(
                "base-v2",
                &[("community", "community-v3")]
            ))
            .as_deref()
        );
        assert_ne!(entry.published_epoch, previous_epoch);
        assert!(entry
            .stats
            .published_graph_report
            .as_ref()
            .is_some_and(|report| {
                report.matches_content(
                    entry
                        .published_graph_content_version
                        .as_deref()
                        .expect("published content version"),
                )
            }));
    }
}
