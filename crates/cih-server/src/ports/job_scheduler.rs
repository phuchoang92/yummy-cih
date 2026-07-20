use async_trait::async_trait;

use crate::domain::error::AppError;
use crate::domain::indexing::{IndexJobSnapshot, IndexJobSpec, IndexSchedulerReceipt};

#[async_trait]
pub(crate) trait IndexJobScheduler: Send + Sync {
    async fn submit(&self, spec: IndexJobSpec) -> Result<IndexSchedulerReceipt, AppError>;

    async fn status(&self, job_id: &str) -> Result<IndexJobSnapshot, AppError>;

    async fn cancel(&self, job_id: &str) -> Result<(), AppError>;
}
