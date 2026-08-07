//! Typed repository-indexing application service.

use std::sync::Arc;

use serde::Serialize;

use crate::domain::error::AppError;
use crate::domain::indexing::{IndexJobSnapshot, IndexJobSpec};
use crate::ports::index_target_resolver::IndexTargetResolver;
use crate::ports::job_scheduler::IndexJobScheduler;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexRepositoryCommand {
    repo_path: String,
    languages: Vec<String>,
    requested_graph_key: String,
}

impl IndexRepositoryCommand {
    pub(crate) fn try_new(
        repo_path: String,
        languages: String,
        graph_key: String,
    ) -> Result<Self, AppError> {
        let repo_path = repo_path.trim();
        if repo_path.is_empty() {
            return Err(AppError::InvalidInput {
                field: "repo_path",
                message: "repository path is required".into(),
            });
        }
        let mut languages: Vec<String> = languages
            .split(',')
            .map(str::trim)
            .filter(|language| !language.is_empty())
            .map(str::to_string)
            .collect();
        languages.sort();
        languages.dedup();
        Ok(Self {
            repo_path: repo_path.to_string(),
            languages,
            requested_graph_key: graph_key.trim().to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexStatusCommand {
    job_id: String,
}

impl IndexStatusCommand {
    pub(crate) fn try_new(job_id: String) -> Result<Self, AppError> {
        Ok(Self {
            job_id: validate_job_id(job_id)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CancelIndexCommand {
    job_id: String,
}

impl CancelIndexCommand {
    pub(crate) fn try_new(job_id: String) -> Result<Self, AppError> {
        Ok(Self {
            job_id: validate_job_id(job_id)?,
        })
    }
}

fn validate_job_id(job_id: String) -> Result<String, AppError> {
    let job_id = job_id.trim();
    if job_id.is_empty() {
        return Err(AppError::InvalidInput {
            field: "job_id",
            message: "job id is required".into(),
        });
    }
    Ok(job_id.to_string())
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct IndexRepositoryOutput {
    job_id: String,
    status: &'static str,
    repo: String,
    message: String,
}

impl IndexRepositoryOutput {
    pub(crate) fn job_id(&self) -> &str {
        &self.job_id
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CancelIndexOutput {
    job_id: String,
    status: &'static str,
    message: String,
}

#[derive(Clone)]
pub(crate) struct IndexingService {
    targets: Arc<dyn IndexTargetResolver>,
    scheduler: Arc<dyn IndexJobScheduler>,
}

impl IndexingService {
    pub(crate) fn new(
        targets: Arc<dyn IndexTargetResolver>,
        scheduler: Arc<dyn IndexJobScheduler>,
    ) -> Self {
        Self { targets, scheduler }
    }

    pub(crate) async fn start(
        &self,
        command: IndexRepositoryCommand,
    ) -> Result<IndexRepositoryOutput, AppError> {
        let target = self
            .targets
            .resolve(&command.repo_path, &command.requested_graph_key)
            .await?;
        let repo = target.canonical_path.display().to_string();
        let receipt = self
            .scheduler
            .submit(IndexJobSpec {
                target,
                languages: command.languages,
            })
            .await?;
        let job_id = receipt.job_id;
        let (status, message) = if receipt.deduplicated {
            (
                "already_active",
                format!(
                    "This repo already has an active index job. Poll \
                     index_status(job_id=\"{job_id}\")."
                ),
            )
        } else {
            (
                "queued",
                format!("Indexing queued. Poll with index_status(job_id=\"{job_id}\")."),
            )
        };
        Ok(IndexRepositoryOutput {
            job_id,
            status,
            repo,
            message,
        })
    }

    pub(crate) async fn status(
        &self,
        command: IndexStatusCommand,
    ) -> Result<IndexJobSnapshot, AppError> {
        self.scheduler.status(&command.job_id).await
    }

    pub(crate) async fn cancel(
        &self,
        command: CancelIndexCommand,
    ) -> Result<CancelIndexOutput, AppError> {
        self.scheduler.cancel(&command.job_id).await?;
        Ok(CancelIndexOutput {
            message: format!(
                "Cancellation signalled. Poll index_status(job_id=\"{}\") for the final state.",
                command.job_id
            ),
            job_id: command.job_id,
            status: "cancelling",
        })
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::domain::indexing::{IndexSchedulerReceipt, ResolvedRepoTarget};

    struct FixedTargetResolver {
        target: ResolvedRepoTarget,
    }

    #[async_trait]
    impl IndexTargetResolver for FixedTargetResolver {
        async fn resolve(
            &self,
            _repo_path: &str,
            _requested_graph_key: &str,
        ) -> Result<ResolvedRepoTarget, AppError> {
            Ok(self.target.clone())
        }
    }

    struct RecordingScheduler {
        submitted: Mutex<Vec<IndexJobSpec>>,
        receipt: IndexSchedulerReceipt,
    }

    #[async_trait]
    impl IndexJobScheduler for RecordingScheduler {
        async fn submit(&self, spec: IndexJobSpec) -> Result<IndexSchedulerReceipt, AppError> {
            self.submitted.lock().unwrap().push(spec);
            Ok(self.receipt.clone())
        }

        async fn status(&self, _job_id: &str) -> Result<IndexJobSnapshot, AppError> {
            Ok(IndexJobSnapshot::Running { started_at_secs: 7 })
        }

        async fn cancel(&self, _job_id: &str) -> Result<(), AppError> {
            Ok(())
        }
    }

    fn service(deduplicated: bool) -> (IndexingService, Arc<RecordingScheduler>) {
        let scheduler = Arc::new(RecordingScheduler {
            submitted: Mutex::new(Vec::new()),
            receipt: IndexSchedulerReceipt {
                job_id: "idx-1".into(),
                deduplicated,
            },
        });
        (
            IndexingService::new(
                Arc::new(FixedTargetResolver {
                    target: ResolvedRepoTarget {
                        canonical_path: PathBuf::from("/repos/demo"),
                        graph_key: "demo".into(),
                    },
                }),
                scheduler.clone(),
            ),
            scheduler,
        )
    }

    #[test]
    fn commands_validate_and_normalize_values() {
        let command = IndexRepositoryCommand::try_new(
            " /repos/demo ".into(),
            "rust, java,rust, ".into(),
            " demo ".into(),
        )
        .unwrap();
        assert_eq!(command.repo_path, "/repos/demo");
        assert_eq!(command.languages, vec!["java", "rust"]);
        assert_eq!(command.requested_graph_key, "demo");
        assert!(IndexRepositoryCommand::try_new(" ".into(), String::new(), String::new()).is_err());
        assert!(IndexStatusCommand::try_new(" ".into()).is_err());
        assert!(CancelIndexCommand::try_new(String::new()).is_err());
    }

    #[tokio::test]
    async fn start_returns_stable_typed_shape_and_normalized_spec() {
        let (service, scheduler) = service(false);
        let output = service
            .start(
                IndexRepositoryCommand::try_new(
                    "/repos/demo".into(),
                    "rust,java,rust".into(),
                    String::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let json = serde_json::to_value(output).unwrap();
        assert_eq!(json["job_id"], "idx-1");
        assert_eq!(json["status"], "queued");
        assert_eq!(json["repo"], "/repos/demo");
        assert!(json["message"].as_str().unwrap().contains("index_status"));

        let submitted = scheduler.submitted.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].languages, vec!["java", "rust"]);
        assert_eq!(submitted[0].target.graph_key, "demo");
    }

    #[tokio::test]
    async fn duplicate_and_cancel_outputs_preserve_wire_contract() {
        let (service, _) = service(true);
        let started = service
            .start(
                IndexRepositoryCommand::try_new("/repos/demo".into(), String::new(), String::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(started).unwrap()["status"],
            "already_active"
        );

        let cancelled = service
            .cancel(CancelIndexCommand::try_new(" idx-1 ".into()).unwrap())
            .await
            .unwrap();
        let json = serde_json::to_value(cancelled).unwrap();
        assert_eq!(json["job_id"], "idx-1");
        assert_eq!(json["status"], "cancelling");
    }

    #[tokio::test]
    async fn status_returns_typed_snapshot() {
        let (service, _) = service(false);
        let status = service
            .status(IndexStatusCommand::try_new("idx-1".into()).unwrap())
            .await
            .unwrap();
        assert_eq!(status.status_label(), "running");
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({"status": "running", "started_at_secs": 7})
        );
    }
}
