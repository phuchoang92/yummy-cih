//! `add_resolve_pattern` / `list_resolve_patterns` — let a connected agent teach CIH a repo's own
//! framework conventions by writing rules to `<repo>/cih.patterns.toml`, then (optionally) kicking a
//! re-index so the deterministic engine re-applies them. The tool only persists rules + reindexes;
//! all rule *application* stays in the engine.

use rmcp::{model::CallToolResult, ErrorData as McpError};

use cih_patterns::{load_patterns, patterns_path, to_toml, RouteRule};

use crate::args::{AddResolvePatternArgs, ListResolvePatternsArgs};
use crate::indexing;
use crate::jobs::Jobs;
use crate::utils::json_result;

/// Resolve a repo name/path (or the active graph key) to its filesystem root via the registry.
fn repo_root(repo: &str, graph_key: &str) -> Result<String, McpError> {
    let reg = cih_core::Registry::load();
    if reg.entries.is_empty() {
        return Err(McpError::invalid_params(
            "no repos in registry — run index_repo first".to_string(),
            None,
        ));
    }
    let entry = if repo.is_empty() {
        reg.entries
            .iter()
            .find(|e| e.graph_key == graph_key)
            .cloned()
    } else {
        reg.find(repo).cloned()
    };
    entry.map(|e| e.path).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "repo '{repo}' not found in registry — index it first, or pass an explicit repo"
            ),
            None,
        )
    })
}

/// Add a resolve pattern to `<repo>/cih.patterns.toml`, de-duping, then optionally re-index.
pub async fn add_resolve_pattern(
    backend: &str,
    falkor_url: &str,
    graph_key: &str,
    jobs: &Jobs,
    args: AddResolvePatternArgs,
) -> Result<CallToolResult, McpError> {
    if args.kind != "route" {
        return Err(McpError::invalid_params(
            format!(
                "unsupported pattern kind '{}': only \"route\" is supported",
                args.kind
            ),
            None,
        ));
    }
    if args.annotation.trim().is_empty() {
        return Err(McpError::invalid_params(
            "`annotation` is required (the annotation name to match, without @)".to_string(),
            None,
        ));
    }

    let root = repo_root(&args.repo, graph_key)?;
    let repo_path = std::path::Path::new(&root);

    let rule = RouteRule {
        annotation: args.annotation.trim().to_string(),
        path_attr: nonempty(&args.path_attr).unwrap_or_else(|| "value".to_string()),
        method: nonempty(&args.method),
        method_attr: nonempty(&args.method_attr),
        class_prefix_annotation: nonempty(&args.class_prefix_annotation),
        class_prefix_attr: "value".to_string(),
    };

    let mut rules = load_patterns(repo_path);
    let added = rules.add_route(rule);
    let path = patterns_path(repo_path);
    if added {
        std::fs::write(&path, to_toml(&rules)).map_err(|e| {
            McpError::internal_error(format!("failed to write {}: {e}", path.display()), None)
        })?;
    }

    // Trigger a background re-index so the live graph reflects the new pattern.
    // No explicit graph key: `root` came from the registry, so the job resolves
    // to that entry's own key (never the server's primary key).
    let mut job_id = None;
    if args.reindex {
        if let Ok((id, _)) =
            indexing::start_index_job(backend, falkor_url, "", jobs, &root, "").await
        {
            job_id = Some(id);
        }
    }

    json_result(&serde_json::json!({
        "added": added,
        "route_rules": rules.routes.len(),
        "patterns_file": path.display().to_string(),
        "reindex_job_id": job_id,
        "message": if added {
            "Pattern added. Poll index_status(job_id=...) if a reindex was started, then re-run route_map."
        } else {
            "Pattern already present (no change)."
        },
    }))
}

/// Return the current resolve patterns for a repo.
pub async fn list_resolve_patterns(
    graph_key: &str,
    args: ListResolvePatternsArgs,
) -> Result<CallToolResult, McpError> {
    let root = repo_root(&args.repo, graph_key)?;
    let rules = load_patterns(std::path::Path::new(&root));
    json_result(&serde_json::json!({
        "patterns_file": patterns_path(std::path::Path::new(&root)).display().to_string(),
        "routes": rules.routes,
    }))
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
