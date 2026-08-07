use crate::domain::observability::RequestCompletion;

pub(crate) trait ObservabilityPort: Send + Sync {
    fn record_request_completion(&self, event: RequestCompletion);
}
