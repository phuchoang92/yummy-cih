//! HTTP authentication, health, readiness, and shutdown handling.

use axum::{
    extract::State,
    http::{header::AUTHORIZATION, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;
use std::time::Instant;

use crate::application::admin::OperationalMetricsService;
use crate::application::browser::ReadinessService;
use crate::domain::observability::{RequestCompletion, RequestErrorKind, RequestTransport};
use crate::ports::observability::ObservabilityPort;

static HTTP_REQUEST_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub async fn observability_middleware(
    State(observability): State<Arc<dyn ObservabilityPort>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 128)
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "http-{}",
                HTTP_REQUEST_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        });
    let capability = http_capability(request.method(), request.uri().path());
    let (response, queue_wait_ms) =
        crate::ports::blocking_runtime::track_queue_wait(next.run(request)).await;
    let status = response.status();
    let response_bytes = response
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    let error_kind = match status.as_u16() {
        408 => Some(RequestErrorKind::Timeout),
        429 => Some(RequestErrorKind::Overload),
        400..=499 => Some(RequestErrorKind::Protocol),
        503 => Some(RequestErrorKind::Dependency),
        500..=599 => Some(RequestErrorKind::Internal),
        _ => None,
    };
    observability.record_request_completion(RequestCompletion {
        request_id,
        transport: RequestTransport::Http,
        capability,
        repository_id: None,
        duration_ms: started.elapsed().as_millis() as u64,
        queue_wait_ms: Some(queue_wait_ms),
        result_count: None,
        response_bytes,
        completeness: None,
        error_kind,
    });
    response
}

fn http_capability(method: &axum::http::Method, path: &str) -> String {
    let route = if path.starts_with("/wiki/") {
        "/wiki/*"
    } else if path.starts_with("/graph/") {
        "/graph/*"
    } else if path == "/graph" {
        "/graph"
    } else if path == "/ready" {
        "/ready"
    } else if path == "/health" {
        "/health"
    } else {
        "/other"
    };
    format!("{} {route}", method.as_str())
}

pub async fn auth_middleware(
    State(token): State<Option<String>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if let Some(expected) = &token {
        let provided = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // Use constant-time comparison to prevent timing side-channel attacks
        // that could allow an attacker to recover the token byte-by-byte.
        let authed = match provided {
            Some(tok) => constant_time_eq::constant_time_eq(tok.as_bytes(), expected.as_bytes()),
            None => false,
        };
        if !authed {
            return (StatusCode::UNAUTHORIZED, "Unauthorized\n").into_response();
        }
    }
    next.run(request).await
}

pub async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

pub async fn ready_handler(State(service): State<ReadinessService>) -> impl IntoResponse {
    let report = service.check().await;
    readiness_response(report)
}

pub async fn operational_metrics_handler(
    State(service): State<OperationalMetricsService>,
) -> impl IntoResponse {
    Json(service.snapshot().await)
}

fn readiness_response(report: crate::application::browser::ReadinessReport) -> Response {
    if report.is_ready() {
        (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "degraded",
                "issues": report.issues
            })),
        )
            .into_response()
    }
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = ctrl_c => {},
                    _ = sigterm.recv() => {},
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler; falling back to SIGINT only");
                ctrl_c.await;
            }
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;

    tracing::info!("shutdown signal received, draining connections");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::browser::ReadinessReport;

    #[test]
    fn readiness_report_controls_http_status() {
        let ready = readiness_response(ReadinessReport { issues: Vec::new() });
        assert_eq!(ready.status(), StatusCode::OK);

        let degraded = readiness_response(ReadinessReport {
            issues: vec!["graph store unreachable"],
        });
        assert_eq!(degraded.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
