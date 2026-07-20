//! Wiki search HTTP adapter.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::application::wiki_search::{WikiSearchCommand, WikiSearchService};
use crate::domain::error::AppError;

#[derive(Debug, Deserialize)]
struct WikiSearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    repo: String,
    role: Option<String>,
    kind: Option<String>,
    feature: Option<String>,
    limit: Option<usize>,
}

pub(crate) fn router(service: WikiSearchService) -> Router {
    Router::new()
        .route("/wiki/search", get(wiki_search_handler))
        .with_state(service)
}

async fn wiki_search_handler(
    State(service): State<WikiSearchService>,
    Query(params): Query<WikiSearchParams>,
) -> Response {
    let command = match WikiSearchCommand::try_new(
        params.q,
        params.repo,
        params.role,
        params.kind,
        params.feature,
        params.limit,
    ) {
        Ok(command) => command,
        Err(error) => return app_error_response(error),
    };
    match service.search(command).await {
        Ok(output) => Json(output).into_response(),
        Err(error) => app_error_response(error),
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn app_error_response(error: AppError) -> Response {
    match error {
        AppError::InvalidInput { field, message } => error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid {field}: {message}"),
        ),
        AppError::NotFound { entity, key } => {
            let status = if entity == "wiki" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            error_response(status, &format!("{entity} '{key}' not found"))
        }
        AppError::Unavailable {
            dependency,
            message,
            retryable,
        } => {
            tracing::error!(
                dependency,
                error = %message,
                retryable,
                "wiki repository dependency unavailable"
            );
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "{dependency} unavailable{}",
                    if retryable { "; retry shortly" } else { "" }
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_mapping_distinguishes_validation_and_missing_wiki() {
        let invalid = app_error_response(AppError::InvalidInput {
            field: "q",
            message: "query parameter is required".into(),
        });
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let missing = app_error_response(AppError::NotFound {
            entity: "wiki",
            key: "demo".into(),
        });
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
