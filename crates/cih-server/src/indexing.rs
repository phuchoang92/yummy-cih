use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::IndexRepoArgs;
use crate::jobs::{evict_terminal, find_engine_binary, new_job_id, unix_now_secs, JobState, Jobs};
use crate::utils::json_result;

pub async fn index_repo(
    backend: &str,
    falkor_url: &str,
    jobs: &Jobs,
    args: IndexRepoArgs,
) -> Result<CallToolResult, McpError> {
    let (job_id, canonical) = start_index_job(
        backend,
        falkor_url,
        &args.graph_key,
        jobs,
        &args.repo_path,
        &args.languages,
    )
    .await?;
    json_result(&serde_json::json!({
        "job_id": job_id,
        "status": "running",
        "repo": canonical,
        "message": format!("Indexing started. Poll with index_status(job_id=\"{job_id}\")."),
    }))
}

/// Resolve the graph key an index job for `canonical` must target: a
/// registered path always uses its own registry key, an unregistered path
/// requires an explicit new key, and a key owned by a different repo is
/// rejected. The server's primary key is never applied implicitly — doing so
/// loaded any `repo_path` into the primary graph.
fn resolve_target_graph_key(
    reg: &cih_core::Registry,
    canonical: &std::path::Path,
    explicit: &str,
) -> Result<String, String> {
    let explicit = explicit.trim();
    let owner = reg.entries.iter().find(|e| {
        std::path::Path::new(&e.path)
            .canonicalize()
            .map(|p| p == canonical)
            .unwrap_or_else(|_| std::path::Path::new(&e.path) == canonical)
    });
    match owner {
        Some(entry) => {
            if !explicit.is_empty() && explicit != entry.graph_key {
                return Err(format!(
                    "repo '{}' is registered under graph key '{}'; omit `graph_key` or pass \
                     that key (got '{explicit}')",
                    entry.name, entry.graph_key
                ));
            }
            Ok(entry.graph_key.clone())
        }
        None => {
            if explicit.is_empty() {
                return Err(
                    "repo is not in the registry; pass an explicit `graph_key` to index it \
                     under (a new key — not one owned by another repo)"
                        .to_string(),
                );
            }
            if let Some(other) = reg.entries.iter().find(|e| e.graph_key == explicit) {
                return Err(format!(
                    "graph key '{explicit}' is already owned by repo '{}' ({}); choose a new key",
                    other.name, other.path
                ));
            }
            Ok(explicit.to_string())
        }
    }
}

/// Validate `repo_path`, resolve the target graph key (see
/// [`resolve_target_graph_key`]), spawn a background `cih-engine analyze`, and
/// return `(job_id, canonical repo path)`. Shared by the `index_repo` tool and
/// `add_resolve_pattern`'s reindex. `requested_graph_key` is the caller's
/// explicit key ("" = resolve from the registry).
pub async fn start_index_job(
    backend: &str,
    falkor_url: &str,
    requested_graph_key: &str,
    jobs: &Jobs,
    repo_path: &str,
    languages: &str,
) -> Result<(String, String), McpError> {
    let repo = std::path::Path::new(repo_path);
    if !repo.is_dir() {
        return Err(McpError::invalid_params(
            format!("'{repo_path}' does not exist or is not a directory"),
            None,
        ));
    }
    let canonical = repo.canonicalize().map_err(|e| {
        McpError::invalid_params(format!("cannot canonicalize repo_path: {e}"), None)
    })?;
    let repo_str = canonical.display().to_string();
    // Fresh (non-cached) registry read: indexing is rare and a just-finished
    // job may have added the entry this resolution depends on.
    let graph_key =
        resolve_target_graph_key(&cih_core::Registry::load(), &canonical, requested_graph_key)
            .map_err(|e| McpError::invalid_params(e, None))?;

    let job_id = new_job_id();
    let started_at_secs = unix_now_secs();
    {
        let mut jobs_lock = jobs.write().await;
        jobs_lock.insert(job_id.clone(), JobState::Running { started_at_secs });
        evict_terminal(&mut jobs_lock);
    }

    let engine = find_engine_binary();
    let backend = backend.to_string();
    let falkor_url = falkor_url.to_string();
    let jobs = jobs.clone();
    let job_id2 = job_id.clone();
    let languages = languages.to_string();

    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new(&engine);
        cmd.arg("analyze")
            .arg(&repo_str)
            .arg("--all")
            .env("CIH_GRAPH_BACKEND", &backend)
            .env("FALKOR_URL", &falkor_url)
            .env("CIH_GRAPH_KEY", &graph_key)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn,cih_engine=info")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if !languages.is_empty() {
            for lang in languages.split(',') {
                let l = lang.trim();
                if !l.is_empty() {
                    cmd.arg("--language").arg(l);
                }
            }
        }

        let result = cmd.output().await;
        let finished_at_secs = unix_now_secs();
        let mut jobs = jobs.write().await;
        match result {
            Ok(out) if out.status.success() => {
                let output = String::from_utf8_lossy(&out.stdout).trim().to_string();
                jobs.insert(
                    job_id2,
                    JobState::Done {
                        started_at_secs,
                        finished_at_secs,
                        output,
                    },
                );
            }
            Ok(out) => {
                let stderr: String = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .filter(|l| !l.contains('\x1b'))
                    .collect::<Vec<_>>()
                    .join("\n");
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let error = format!(
                    "cih-engine exited {}: {}\n{}",
                    out.status.code().unwrap_or(-1),
                    stderr.trim(),
                    stdout,
                );
                jobs.insert(
                    job_id2,
                    JobState::Failed {
                        started_at_secs,
                        finished_at_secs,
                        error,
                    },
                );
            }
            Err(e) => {
                jobs.insert(
                    job_id2,
                    JobState::Failed {
                        started_at_secs,
                        finished_at_secs,
                        error: format!("failed to launch {}: {e}", engine.display()),
                    },
                );
            }
        }
    });

    Ok((job_id, canonical.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::resolve_target_graph_key;
    use cih_core::{Registry, RegistryEntry};

    fn entry(name: &str, path: &str, graph_key: &str) -> RegistryEntry {
        RegistryEntry {
            name: name.to_string(),
            path: path.to_string(),
            graph_key: graph_key.to_string(),
            artifacts_dir: String::new(),
            community_artifacts_dir: None,
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        }
    }

    /// A registered path uses its own registry key — never the server primary.
    #[test]
    fn registered_path_uses_its_registry_key() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("svc", &canonical.display().to_string(), "svc-key")],
        };
        assert_eq!(
            resolve_target_graph_key(&reg, &canonical, "").unwrap(),
            "svc-key"
        );
        // An explicit key matching the registry entry is accepted too.
        assert_eq!(
            resolve_target_graph_key(&reg, &canonical, "svc-key").unwrap(),
            "svc-key"
        );
    }

    #[test]
    fn registered_path_rejects_conflicting_explicit_key() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("svc", &canonical.display().to_string(), "svc-key")],
        };
        let err = resolve_target_graph_key(&reg, &canonical, "primary").unwrap_err();
        assert!(
            err.contains("registered under graph key 'svc-key'"),
            "{err}"
        );
    }

    /// The S9 regression: an unregistered path must not silently land under
    /// any implicit key — the caller has to name a fresh one.
    #[test]
    fn unregistered_path_requires_explicit_key() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("other", "/somewhere/else", "primary")],
        };
        let err = resolve_target_graph_key(&reg, &canonical, "").unwrap_err();
        assert!(err.contains("pass an explicit `graph_key`"), "{err}");
        assert_eq!(
            resolve_target_graph_key(&reg, &canonical, "new-key").unwrap(),
            "new-key"
        );
    }

    #[test]
    fn unregistered_path_rejects_key_owned_by_another_repo() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let reg = Registry {
            entries: vec![entry("other", "/somewhere/else", "primary")],
        };
        let err = resolve_target_graph_key(&reg, &canonical, "primary").unwrap_err();
        assert!(err.contains("already owned by repo 'other'"), "{err}");
    }
}
