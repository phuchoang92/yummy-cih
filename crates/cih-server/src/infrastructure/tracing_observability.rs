use crate::domain::observability::RequestCompletion;
use crate::ports::observability::ObservabilityPort;

#[derive(Clone, Default)]
pub(crate) struct TracingObservability;

impl ObservabilityPort for TracingObservability {
    fn record_request_completion(&self, event: RequestCompletion) {
        tracing::info!(
            request_id = %event.request_id,
            transport = ?event.transport,
            capability = %event.capability,
            repository_id = event.repository_id.as_deref().unwrap_or(""),
            duration_ms = event.duration_ms,
            queue_wait_ms = event.queue_wait_ms,
            result_count = event.result_count,
            response_bytes = event.response_bytes,
            completeness = event.completeness.as_deref().unwrap_or("unknown"),
            error_kind = ?event.error_kind,
            "request_completed"
        );
    }
}
