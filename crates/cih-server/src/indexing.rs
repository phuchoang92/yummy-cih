use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::IndexRepoArgs;
use crate::jobs::{evict_terminal, find_engine_binary, new_job_id, unix_now_secs, JobState, Jobs};
use crate::utils::json_result;

pub async fn index_repo(
    backend: &str,
    falkor_url: &str,
    graph_key: &str,
    jobs: &Jobs,
    args: IndexRepoArgs,
) -> Result<CallToolResult, McpError> {
    let (job_id, canonical) = start_index_job(
        backend,
        falkor_url,
        graph_key,
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

/// Validate `repo_path`, spawn a background `cih-engine analyze`, and return `(job_id, canonical
/// repo path)`. Shared by the `index_repo` tool and `add_resolve_pattern`'s reindex.
pub async fn start_index_job(
    backend: &str,
    falkor_url: &str,
    graph_key: &str,
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
    let graph_key = graph_key.to_string();
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
