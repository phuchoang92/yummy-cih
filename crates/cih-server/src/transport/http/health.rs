//! HTTP authentication, health, readiness, and shutdown handling.

use axum::{
    extract::State,
    http::{header::AUTHORIZATION, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::application::browser::ReadinessService;

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
