//! Repository indexing job values shared by application and infrastructure.

use std::path::PathBuf;

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedRepoTarget {
    pub(crate) canonical_path: PathBuf,
    pub(crate) graph_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexJobSpec {
    pub(crate) target: ResolvedRepoTarget,
    pub(crate) languages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexSchedulerReceipt {
    pub(crate) job_id: String,
    pub(crate) deduplicated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum IndexJobSnapshot {
    Queued {
        queued_at_secs: u64,
    },
    Running {
        started_at_secs: u64,
    },
    Done {
        started_at_secs: u64,
        finished_at_secs: u64,
        output: String,
        output_truncated: bool,
    },
    Failed {
        started_at_secs: u64,
        finished_at_secs: u64,
        error: String,
    },
    TimedOut {
        started_at_secs: u64,
        finished_at_secs: u64,
        timeout_secs: u64,
    },
    Cancelled {
        cancelled_at_secs: u64,
    },
}

impl IndexJobSnapshot {
    pub(crate) fn status_label(&self) -> &'static str {
        match self {
            Self::Queued { .. } => "queued",
            Self::Running { .. } => "running",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
            Self::TimedOut { .. } => "timed_out",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}
