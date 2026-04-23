//! Unified HTTP API error type. Wraps the various module-level error types
//! (`AddError`, `MutationError`, `fs_browse::ListError`) plus a catch-all for
//! internal anyhow failures and client-side bad requests.
//!
//! Implements [`IntoResponse`] so handlers can return
//! `Result<Json<T>, ApiError>` and rely on `?` for the error path.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::{collections::MutationError, directories::AddError, fs_browse::ListError};

/// Common error returned by HTTP handlers.
#[derive(Debug)]
pub enum ApiError {
    /// 404 — resource not found. Error code included in the JSON body.
    NotFound(&'static str),
    /// 400 — client sent bad data.
    BadRequest { code: &'static str, message: String },
    /// Typed directory-add error mapped to its canonical status.
    Add(AddError),
    /// Typed collection-mutation error mapped to its canonical status.
    Mutation(MutationError),
    /// Typed fs-browse error mapped to its canonical status.
    FsList(ListError),
    /// Catch-all for internal failures. Logged; surfaced to the client as 500.
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound(code) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": code })),
            )
                .into_response(),
            ApiError::BadRequest { code, message } => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": code, "message": message })),
            )
                .into_response(),
            ApiError::Add(err) => (err.status(), Json(err)).into_response(),
            ApiError::Mutation(err) => (err.status(), Json(err)).into_response(),
            ApiError::FsList(err) => (err.status(), Json(err)).into_response(),
            ApiError::Internal(err) => {
                tracing::error!(error = %format!("{err:#}"), "internal api error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "internal",
                        "message": err.to_string(),
                    })),
                )
                    .into_response()
            }
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError::Internal(err)
    }
}

impl From<AddError> for ApiError {
    fn from(err: AddError) -> Self {
        ApiError::Add(err)
    }
}

impl From<MutationError> for ApiError {
    fn from(err: MutationError) -> Self {
        ApiError::Mutation(err)
    }
}

impl From<ListError> for ApiError {
    fn from(err: ListError) -> Self {
        ApiError::FsList(err)
    }
}

/// Short constructor for bad-request errors.
pub fn bad_request(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::BadRequest {
        code,
        message: message.into(),
    }
}
