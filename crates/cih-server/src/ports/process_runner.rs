//! Process-execution boundary used by the indexing scheduler.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

/// Complete, explicit description of one engine invocation.
#[derive(Clone, Debug)]
pub(crate) struct EngineProcessSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) current_dir: PathBuf,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) deadline: Duration,
    pub(crate) output_cap: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EngineProcessOutcome {
    Exited {
        code: i32,
        success: bool,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
    TimedOut,
    Cancelled,
    LaunchFailed(String),
}

#[async_trait]
pub(crate) trait EngineProcessRunner: Send + Sync {
    async fn run(
        &self,
        spec: EngineProcessSpec,
        cancel: watch::Receiver<bool>,
    ) -> EngineProcessOutcome;
}
