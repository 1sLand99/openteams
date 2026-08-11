use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Serialize;
use services::services::output_validation::{
    MAX_OUTPUT_VALIDATION_REQUEST_BYTES, OutputValidationRequest, validate_output_request,
};

use crate::DeploymentImpl;

#[derive(Debug, Serialize)]
struct OutputValidationRequestError {
    error: String,
}

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/output-validation", post(validate_output))
        .layer(DefaultBodyLimit::max(MAX_OUTPUT_VALIDATION_REQUEST_BYTES))
}

async fn validate_output(
    payload: Result<Json<OutputValidationRequest>, JsonRejection>,
) -> Response {
    match payload {
        Ok(Json(request)) => Json(validate_output_request(&request)).into_response(),
        Err(rejection) => {
            let status = if rejection.status() == StatusCode::UNPROCESSABLE_ENTITY {
                StatusCode::BAD_REQUEST
            } else {
                rejection.status()
            };
            (
                status,
                Json(OutputValidationRequestError {
                    error: rejection.body_text(),
                }),
            )
                .into_response()
        }
    }
}
