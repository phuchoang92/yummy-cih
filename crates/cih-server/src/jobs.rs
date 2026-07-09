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

pub fn new_job_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("idx-{ms}")
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
