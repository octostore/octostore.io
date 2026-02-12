use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Authentication failed")]
    Unauthorized,
    
    #[error("Bearer token required")]
    MissingAuth,
    
    #[error("Lock not found: {name}")]
    LockNotFound { name: String },
    
    #[error("Lock already held by another user")]
    LockHeld,
    
    #[error("Invalid lease ID")]
    InvalidLeaseId,
    
    #[error("Lock limit exceeded (max 100 per user)")]
    LockLimitExceeded,
    
    #[error("Invalid TTL: {reason}")]
    InvalidTtl { reason: String },
    
    #[error("Invalid lock name: {reason}")]
    InvalidLockName { reason: String },
    
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    
    #[error("Resource not found: {0}")]
    NotFound(String),
    
    #[error("Conflict: {0}")]
    Conflict(String),
    
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    
    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),
    
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    
    #[error("UUID parsing error: {0}")]
    Uuid(#[from] uuid::Error),
    
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "Authentication failed"),
            AppError::MissingAuth => (StatusCode::UNAUTHORIZED, "Authorization header required"),
            AppError::LockNotFound { .. } => (StatusCode::NOT_FOUND, "Lock not found"),
            AppError::LockHeld => (StatusCode::CONFLICT, "Lock is held by another user"),
            AppError::InvalidLeaseId => (StatusCode::BAD_REQUEST, "Invalid lease ID"),
            AppError::LockLimitExceeded => (StatusCode::FORBIDDEN, "Lock limit exceeded"),
            AppError::InvalidTtl { .. } => (StatusCode::BAD_REQUEST, "Invalid TTL"),
            AppError::InvalidLockName { .. } => (StatusCode::BAD_REQUEST, "Invalid lock name"),
            AppError::InvalidInput(_) => (StatusCode::BAD_REQUEST, "Invalid input"),
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "Resource not found"),
            AppError::Conflict(_) => (StatusCode::CONFLICT, "Conflict"),
            AppError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Database error"),
            AppError::HttpClient(_) => (StatusCode::BAD_GATEWAY, "External service error"),
            AppError::Json(_) => (StatusCode::BAD_REQUEST, "JSON parsing error"),
            AppError::Uuid(_) => (StatusCode::BAD_REQUEST, "Invalid UUID"),
            AppError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
        };

        let body = Json(json!({
            "error": error_message,
            "details": self.to_string()
        }));

        (status, body).into_response()
    }
}

pub type Result<T> = std::result::Result<T, AppError>;