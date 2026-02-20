use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Unified error type for all API failures.
///
/// Each variant maps to a specific HTTP status code and produces a consistent
/// JSON error response with `error` and `details` fields.
#[derive(Error, Debug)]
#[allow(dead_code)]
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

    #[error("Forbidden: {0}")]
    Forbidden(String),
    
    #[error("Invalid TTL: {reason}")]
    InvalidTtl { reason: String },
    
    #[error("Invalid lock name: {reason}")]
    InvalidLockName { reason: String },
    
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    
    #[error("Session not found")]
    SessionNotFound,

    #[error("Session expired")]
    SessionExpired,

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
            AppError::Forbidden(_) => (StatusCode::FORBIDDEN, "Forbidden"),
            AppError::InvalidTtl { .. } => (StatusCode::BAD_REQUEST, "Invalid TTL"),
            AppError::InvalidLockName { .. } => (StatusCode::BAD_REQUEST, "Invalid lock name"),
            AppError::InvalidInput(_) => (StatusCode::BAD_REQUEST, "Invalid input"),
            AppError::SessionNotFound => (StatusCode::NOT_FOUND, "Session not found"),
            AppError::SessionExpired => (StatusCode::GONE, "Session expired"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::Value;

    #[test]
    fn test_error_display() {
        assert_eq!(AppError::Unauthorized.to_string(), "Authentication failed");
        assert_eq!(AppError::MissingAuth.to_string(), "Bearer token required");
        
        let lock_not_found = AppError::LockNotFound { name: "test-lock".to_string() };
        assert_eq!(lock_not_found.to_string(), "Lock not found: test-lock");
        
        let invalid_ttl = AppError::InvalidTtl { reason: "too large".to_string() };
        assert_eq!(invalid_ttl.to_string(), "Invalid TTL: too large");
        
        let invalid_lock_name = AppError::InvalidLockName { reason: "contains spaces".to_string() };
        assert_eq!(invalid_lock_name.to_string(), "Invalid lock name: contains spaces");
        
        assert_eq!(AppError::InvalidInput("bad data".to_string()).to_string(), "Invalid input: bad data");
        assert_eq!(AppError::NotFound("resource".to_string()).to_string(), "Resource not found: resource");
        assert_eq!(AppError::Conflict("version mismatch".to_string()).to_string(), "Conflict: version mismatch");
    }

    #[test]
    fn test_error_conversions() {
        // Test rusqlite error conversion
        let sqlite_error = rusqlite::Error::InvalidParameterName("test".to_string());
        let app_error = AppError::from(sqlite_error);
        assert!(matches!(app_error, AppError::Database(_)));

        // Note: reqwest::Error testing requires actual HTTP requests or complex mocking
        // The conversion trait implementation works correctly in practice

        // Test serde_json error conversion
        let json_error = serde_json::from_str::<Value>("invalid json").unwrap_err();
        let app_error = AppError::from(json_error);
        assert!(matches!(app_error, AppError::Json(_)));

        // Test uuid error conversion
        let uuid_error = uuid::Uuid::parse_str("invalid-uuid").unwrap_err();
        let app_error = AppError::from(uuid_error);
        assert!(matches!(app_error, AppError::Uuid(_)));

        // Test anyhow error conversion
        let anyhow_error = anyhow::anyhow!("test error");
        let app_error = AppError::from(anyhow_error);
        assert!(matches!(app_error, AppError::Internal(_)));
    }

    #[tokio::test]
    async fn test_error_into_response() {
        // Test each error type's HTTP response
        let test_cases = vec![
            (AppError::Unauthorized, StatusCode::UNAUTHORIZED, "Authentication failed"),
            (AppError::MissingAuth, StatusCode::UNAUTHORIZED, "Authorization header required"),
            (AppError::LockNotFound { name: "test".to_string() }, StatusCode::NOT_FOUND, "Lock not found"),
            (AppError::LockHeld, StatusCode::CONFLICT, "Lock is held by another user"),
            (AppError::InvalidLeaseId, StatusCode::BAD_REQUEST, "Invalid lease ID"),
            (AppError::LockLimitExceeded, StatusCode::FORBIDDEN, "Lock limit exceeded"),
            (AppError::InvalidTtl { reason: "test".to_string() }, StatusCode::BAD_REQUEST, "Invalid TTL"),
            (AppError::InvalidLockName { reason: "test".to_string() }, StatusCode::BAD_REQUEST, "Invalid lock name"),
            (AppError::InvalidInput("test".to_string()), StatusCode::BAD_REQUEST, "Invalid input"),
            (AppError::NotFound("test".to_string()), StatusCode::NOT_FOUND, "Resource not found"),
            (AppError::Conflict("test".to_string()), StatusCode::CONFLICT, "Conflict"),
            (AppError::Internal(anyhow::anyhow!("test")), StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
        ];

        for (error, expected_status, expected_message) in test_cases {
            let response = error.into_response();
            assert_eq!(response.status(), expected_status);
            
            // Extract and verify JSON body
            let (_parts, body) = response.into_parts();
            let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let json: Value = serde_json::from_slice(&body_bytes).unwrap();
            
            assert_eq!(json["error"], expected_message);
            assert!(json["details"].is_string());
        }
    }

    #[test]
    fn test_database_error_conversion() {
        let sqlite_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed".to_string())
        );
        let app_error = AppError::from(sqlite_error);
        
        let response = app_error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_result_type_alias() {
        fn returns_result() -> Result<String> {
            Ok("success".to_string())
        }
        
        fn returns_error() -> Result<String> {
            Err(AppError::Unauthorized)
        }
        
        assert!(returns_result().is_ok());
        assert!(returns_error().is_err());
        assert!(matches!(returns_error().unwrap_err(), AppError::Unauthorized));
    }
}