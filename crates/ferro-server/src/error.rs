use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferro_engine::FerroError;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into() }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "INVALID_PAYLOAD", message)
    }
}

impl From<FerroError> for ApiError {
    fn from(e: FerroError) -> Self {
        let message = e.to_string();
        match &e {
            FerroError::NotFound(_) => ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", message),
            FerroError::Forbidden(_) => ApiError::new(StatusCode::FORBIDDEN, "FORBIDDEN", message),
            FerroError::Unauthorized(_) => ApiError::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", message),
            FerroError::Invalid(_) => ApiError::new(StatusCode::BAD_REQUEST, "INVALID_PAYLOAD", message),
            FerroError::Conflict(_) => ApiError::new(StatusCode::CONFLICT, "CONFLICT", message),
            FerroError::Db(sqlx::Error::RowNotFound) => ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", message),
            FerroError::Db(_) => match e.sqlstate().as_deref() {
                Some("23505") => ApiError::new(StatusCode::CONFLICT, "RECORD_NOT_UNIQUE", message),
                Some("23503") => ApiError::new(StatusCode::BAD_REQUEST, "INVALID_FOREIGN_KEY", message),
                Some("23502") => ApiError::new(StatusCode::BAD_REQUEST, "NOT_NULL_VIOLATION", message),
                Some("22P02" | "22007" | "22008" | "22003" | "22001" | "42804" | "22023") => {
                    ApiError::new(StatusCode::BAD_REQUEST, "INVALID_VALUE", message)
                }
                Some(code) if code.starts_with("22") || code == "23514" => ApiError::new(StatusCode::BAD_REQUEST, "INVALID_VALUE", message),
                _ => {
                    tracing::error!(error = %e, "database error");
                    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", message)
                }
            },
            FerroError::Internal(_) => {
                tracing::error!(error = %e, "internal error");
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", message)
            }
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        FerroError::from(e).into()
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::bad_request(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "errors": [{ "message": self.message, "extensions": { "code": self.code } }] });
        (self.status, Json(body)).into_response()
    }
}
