use std::path::Path;

use cih_core::{git_head, now_rfc3339, Registry, RegistryEntry, RegistryStats};

use crate::analyze::EmitOutcome;
use crate::discover::DiscoverOutcome;

fn repo_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

pub(crate) fn entry_from_analyze(emit: &EmitOutcome, graph_key: &str) -> RegistryEntry {
    let path = emit.scope_file.repo_root.clone();
    RegistryEntry {
        name: repo_name(&path),
        graph_key: graph_key.to_string(),
        artifacts_dir: emit.artifacts_dir.display().to_string(),
        community_artifacts_dir: None,
        indexed_at: now_rfc3339(),
        last_git_head: git_head(Path::new(&path)),
        stats: RegistryStats {
            nodes: emit.node_count,
            edges: emit.edge_count,
            files: emit.parsed_file_count,
            routes: 0, // filled in by discover
            communities: 0,
            processes: 0,
        },
        path,
    }
}

pub(crate) fn update_entry_from_discover(entry: &mut RegistryEntry, disc: &DiscoverOutcome) {
    entry.community_artifacts_dir = Some(disc.artifacts_dir.display().to_string());
    entry.stats.routes = disc.route_count;
    entry.stats.communities = disc.community_count;
    entry.stats.processes = disc.process_count;
}

/// Persist an `EmitOutcome` to the global registry.  Silently logs on failure.
pub(crate) fn persist_analyze(emit: &EmitOutcome, graph_key: &str) {
    let entry = entry_from_analyze(emit, graph_key);
    let mut reg = Registry::load();
    reg.upsert(entry);
    if let Err(e) = reg.save() {
        tracing::warn!(error = %e, "failed to update registry");
    }
}

/// Persist a `DiscoverOutcome` update to the global registry.  Silently logs on failure.
pub(crate) fn persist_discover(repo_path: &Path, disc: &DiscoverOutcome) {
    let path_str = repo_path.display().to_string();
    let mut reg = Registry::load();
    if let Some(entry) = reg.find_mut(&path_str) {
        update_entry_from_discover(entry, disc);
        if let Err(e) = reg.save() {
            tracing::warn!(error = %e, "failed to update registry after discover");
        }
    } else {
        tracing::debug!("registry entry not found for {path_str}; run analyze first");
    }
}
