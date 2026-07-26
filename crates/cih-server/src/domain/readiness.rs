//! Transport-independent backend readiness state and retry guidance.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReadinessState {
    Ready,
    BackendLoading,
    Degraded,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReadinessIssue {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retry_after_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReadinessReport {
    pub(crate) state: ReadinessState,
    pub(crate) issues: Vec<ReadinessIssue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retry_after_ms: Option<u64>,
}

impl ReadinessReport {
    pub(crate) fn new(state: ReadinessState, issues: Vec<ReadinessIssue>) -> Self {
        let retry_after_ms = issues.iter().filter_map(|issue| issue.retry_after_ms).min();
        Self {
            state,
            issues,
            retry_after_ms,
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.state == ReadinessState::Ready && self.issues.is_empty()
    }

    pub(crate) fn backend_issue(&self) -> Option<&ReadinessIssue> {
        self.issues.iter().find(|issue| {
            issue.code.starts_with("BACKEND_")
                || issue.code.starts_with("GRAPH_")
                || issue.code.starts_with("INDEX_")
                || issue.code == "READINESS_PROBE_INVALID"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_exposes_the_earliest_retry_without_claiming_ready() {
        let report = ReadinessReport::new(
            ReadinessState::BackendLoading,
            vec![
                ReadinessIssue {
                    code: "SLOW_RETRY",
                    message: "slow".into(),
                    retryable: true,
                    retry_after_ms: Some(5_000),
                },
                ReadinessIssue {
                    code: "FAST_RETRY",
                    message: "fast".into(),
                    retryable: true,
                    retry_after_ms: Some(1_000),
                },
            ],
        );

        assert!(!report.is_ready());
        assert_eq!(report.retry_after_ms, Some(1_000));
    }
}
