use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::State,
    http::{header::AUTHORIZATION, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use cih_graph_store::GraphStore;

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
        if provided != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, "Unauthorized\n").into_response();
        }
    }
    next.run(request).await
}

pub async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

pub async fn ready_handler(
    State((store, artifacts_dir)): State<(Arc<dyn GraphStore>, Option<PathBuf>)>,
) -> impl IntoResponse {
    let mut issues: Vec<&str> = Vec::new();

    if store.communities().await.is_err() {
        issues.push("graph store unreachable");
    }

    if let Some(dir) = &artifacts_dir {
        if !dir.exists() {
            issues.push("artifacts dir not found");
        }
    }

    if issues.is_empty() {
        (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "degraded", "issues": issues})),
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
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;

    tracing::info!("shutdown signal received, draining connections");
}
