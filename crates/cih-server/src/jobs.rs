use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobState {
    Running {
        started_at_secs: u64,
    },
    Done {
        started_at_secs: u64,
        finished_at_secs: u64,
        output: String,
    },
    Failed {
        started_at_secs: u64,
        finished_at_secs: u64,
        error: String,
    },
}

pub type Jobs = Arc<tokio::sync::RwLock<HashMap<String, JobState>>>;

/// Upper bound on retained job entries. Once exceeded, the oldest terminal
/// (`Done`/`Failed`) jobs are evicted first so the map can't grow unbounded on a
/// long-lived server. `Running` jobs are never evicted.
const MAX_RETAINED_JOBS: usize = 256;

/// Monotonic per-process counter appended to the job id so two `index_repo`
/// calls in the same millisecond can't collide (which previously overwrote the
/// first job's status).
static JOB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn new_job_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = JOB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("idx-{ms}-{seq}")
}

/// Evict the oldest terminal jobs once the map exceeds [`MAX_RETAINED_JOBS`].
/// Call while holding the write lock, right after inserting a new job.
pub fn evict_terminal(jobs: &mut HashMap<String, JobState>) {
    if jobs.len() <= MAX_RETAINED_JOBS {
        return;
    }
    let mut terminal: Vec<(String, u64)> = jobs
        .iter()
        .filter_map(|(id, st)| match st {
            JobState::Done {
                finished_at_secs, ..
            }
            | JobState::Failed {
                finished_at_secs, ..
            } => Some((id.clone(), *finished_at_secs)),
            JobState::Running { .. } => None,
        })
        .collect();
    terminal.sort_by_key(|&(_, finished)| finished);
    let excess = jobs.len() - MAX_RETAINED_JOBS;
    for (id, _) in terminal.into_iter().take(excess) {
        jobs.remove(&id);
    }
}

pub fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Locate the `cih-engine` binary: check alongside this binary first, then fall back to PATH.
pub fn find_engine_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("cih-engine");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("cih-engine")
}
