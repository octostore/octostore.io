use axum::{
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::future::Future;
use thiserror::Error;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

tokio::task_local! {
    static REQUEST_ID: String;
}

pub async fn with_request_id<F>(request_id: String, future: F) -> F::Output
where
    F: Future,
{
    REQUEST_ID.scope(request_id, future).await
}

pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(Clone::clone)
        .unwrap_or_else(|_| format!("req_{}", uuid::Uuid::new_v4().simple()))
}

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

    #[error("Lease is not current")]
    LeaseNotCurrent,

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

    #[error("Capacity exceeded: {details}")]
    CapacityExceeded {
        details: String,
        retry_after_seconds: u64,
    },

    #[error("Rate limit exceeded; retry in {retry_after_seconds} seconds")]
    RateLimited { retry_after_seconds: u64 },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),

    #[error("Upstream service unavailable: {service}")]
    UpstreamUnavailable { service: &'static str },

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("UUID parsing error: {0}")]
    Uuid(#[from] uuid::Error),

    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let request_id = current_request_id();
        let retry_after_seconds = match &self {
            AppError::RateLimited {
                retry_after_seconds,
            }
            | AppError::CapacityExceeded {
                retry_after_seconds,
                ..
            } => Some(*retry_after_seconds),
            _ => None,
        };
        let (status, error_message, code) = match &self {
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Authentication failed",
                "authentication_failed",
            ),
            AppError::MissingAuth => (
                StatusCode::UNAUTHORIZED,
                "Authorization header required",
                "authentication_required",
            ),
            AppError::LockNotFound { .. } => (StatusCode::NOT_FOUND, "Lock not found", "not_found"),
            AppError::LockHeld => (
                StatusCode::CONFLICT,
                "Lock is held by another user",
                "conflict",
            ),
            AppError::InvalidLeaseId => (
                StatusCode::BAD_REQUEST,
                "Lease is not current",
                "lease_not_current",
            ),
            AppError::LeaseNotCurrent => (
                StatusCode::NOT_FOUND,
                "Lease is not current",
                "lease_not_current",
            ),
            AppError::LockLimitExceeded => (
                StatusCode::FORBIDDEN,
                "Lock limit exceeded",
                "lock_limit_exceeded",
            ),
            AppError::Forbidden(_) => (StatusCode::FORBIDDEN, "Forbidden", "forbidden"),
            AppError::InvalidTtl { .. } => (StatusCode::BAD_REQUEST, "Invalid TTL", "invalid_ttl"),
            AppError::InvalidLockName { .. } => (
                StatusCode::BAD_REQUEST,
                "Invalid lock name",
                "invalid_lock_name",
            ),
            AppError::InvalidInput(_) | AppError::Json(_) | AppError::Uuid(_) => {
                (StatusCode::BAD_REQUEST, "Invalid input", "invalid_input")
            }
            AppError::SessionNotFound => (StatusCode::NOT_FOUND, "Session not found", "not_found"),
            AppError::SessionExpired => (StatusCode::GONE, "Session expired", "session_expired"),
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "Resource not found", "not_found"),
            AppError::Conflict(_) => (StatusCode::CONFLICT, "Conflict", "conflict"),
            AppError::CapacityExceeded { .. } => (
                StatusCode::CONFLICT,
                "Capacity exceeded",
                "capacity_exceeded",
            ),
            AppError::RateLimited { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded",
                "rate_limited",
            ),
            AppError::Database(_) | AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
                "internal_error",
            ),
            AppError::HttpClient(_) | AppError::UpstreamUnavailable { .. } => (
                StatusCode::BAD_GATEWAY,
                "Upstream service unavailable",
                "upstream_unavailable",
            ),
        };

        let details = match &self {
            AppError::Database(_)
            | AppError::HttpClient(_)
            | AppError::UpstreamUnavailable { .. }
            | AppError::Internal(_) => error_message.to_string(),
            _ => self.to_string(),
        };

        if status.is_server_error() {
            tracing::error!(
                request_id = %request_id,
                error_code = code,
                "request failed without exposing internal diagnostics"
            );
        }

        let mut body = json!({
            "error": error_message,
            "code": code,
            "details": details,
            "request_id": request_id.clone(),
        });
        if let Some(seconds) = retry_after_seconds {
            body["retry_after_ms"] = json!(seconds.saturating_mul(1_000));
        }

        let body = Json(body);
        let mut response = (status, body).into_response();
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            response.headers_mut().insert(REQUEST_ID_HEADER, value);
        }
        if let Some(seconds) = retry_after_seconds {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
        }
        response
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

        let lock_not_found = AppError::LockNotFound {
            name: "test-lock".to_string(),
        };
        assert_eq!(lock_not_found.to_string(), "Lock not found: test-lock");

        let invalid_ttl = AppError::InvalidTtl {
            reason: "too large".to_string(),
        };
        assert_eq!(invalid_ttl.to_string(), "Invalid TTL: too large");

        let invalid_lock_name = AppError::InvalidLockName {
            reason: "contains spaces".to_string(),
        };
        assert_eq!(
            invalid_lock_name.to_string(),
            "Invalid lock name: contains spaces"
        );

        assert_eq!(
            AppError::InvalidInput("bad data".to_string()).to_string(),
            "Invalid input: bad data"
        );
        assert_eq!(
            AppError::NotFound("resource".to_string()).to_string(),
            "Resource not found: resource"
        );
        assert_eq!(
            AppError::Conflict("version mismatch".to_string()).to_string(),
            "Conflict: version mismatch"
        );
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
        let http_error = reqwest::Client::new()
            .get("://invalid-url")
            .send()
            .await
            .unwrap_err();
        let json_error = serde_json::from_str::<Value>("not-json").unwrap_err();
        let uuid_error = uuid::Uuid::parse_str("not-a-uuid").unwrap_err();
        let test_cases = vec![
            (
                AppError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "Authentication failed",
                "authentication_failed",
            ),
            (
                AppError::MissingAuth,
                StatusCode::UNAUTHORIZED,
                "Authorization header required",
                "authentication_required",
            ),
            (
                AppError::LockNotFound {
                    name: "test".to_string(),
                },
                StatusCode::NOT_FOUND,
                "Lock not found",
                "not_found",
            ),
            (
                AppError::LockHeld,
                StatusCode::CONFLICT,
                "Lock is held by another user",
                "conflict",
            ),
            (
                AppError::InvalidLeaseId,
                StatusCode::BAD_REQUEST,
                "Lease is not current",
                "lease_not_current",
            ),
            (
                AppError::LeaseNotCurrent,
                StatusCode::NOT_FOUND,
                "Lease is not current",
                "lease_not_current",
            ),
            (
                AppError::LockLimitExceeded,
                StatusCode::FORBIDDEN,
                "Lock limit exceeded",
                "lock_limit_exceeded",
            ),
            (
                AppError::Forbidden("test".to_string()),
                StatusCode::FORBIDDEN,
                "Forbidden",
                "forbidden",
            ),
            (
                AppError::InvalidTtl {
                    reason: "test".to_string(),
                },
                StatusCode::BAD_REQUEST,
                "Invalid TTL",
                "invalid_ttl",
            ),
            (
                AppError::InvalidLockName {
                    reason: "test".to_string(),
                },
                StatusCode::BAD_REQUEST,
                "Invalid lock name",
                "invalid_lock_name",
            ),
            (
                AppError::InvalidInput("test".to_string()),
                StatusCode::BAD_REQUEST,
                "Invalid input",
                "invalid_input",
            ),
            (
                AppError::Json(json_error),
                StatusCode::BAD_REQUEST,
                "Invalid input",
                "invalid_input",
            ),
            (
                AppError::Uuid(uuid_error),
                StatusCode::BAD_REQUEST,
                "Invalid input",
                "invalid_input",
            ),
            (
                AppError::SessionNotFound,
                StatusCode::NOT_FOUND,
                "Session not found",
                "not_found",
            ),
            (
                AppError::SessionExpired,
                StatusCode::GONE,
                "Session expired",
                "session_expired",
            ),
            (
                AppError::NotFound("test".to_string()),
                StatusCode::NOT_FOUND,
                "Resource not found",
                "not_found",
            ),
            (
                AppError::Conflict("test".to_string()),
                StatusCode::CONFLICT,
                "Conflict",
                "conflict",
            ),
            (
                AppError::CapacityExceeded {
                    details: "retry later".to_string(),
                    retry_after_seconds: 30,
                },
                StatusCode::CONFLICT,
                "Capacity exceeded",
                "capacity_exceeded",
            ),
            (
                AppError::RateLimited {
                    retry_after_seconds: 30,
                },
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded",
                "rate_limited",
            ),
            (
                AppError::HttpClient(http_error),
                StatusCode::BAD_GATEWAY,
                "Upstream service unavailable",
                "upstream_unavailable",
            ),
            (
                AppError::UpstreamUnavailable {
                    service: "GitHub OAuth token exchange",
                },
                StatusCode::BAD_GATEWAY,
                "Upstream service unavailable",
                "upstream_unavailable",
            ),
            (
                AppError::Database(rusqlite::Error::InvalidQuery),
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
                "internal_error",
            ),
            (
                AppError::Internal(anyhow::anyhow!("test")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
                "internal_error",
            ),
        ];

        for (error, expected_status, expected_message, expected_code) in test_cases {
            let response = error.into_response();
            assert_eq!(response.status(), expected_status);

            // Extract and verify JSON body
            let (parts, body) = response.into_parts();
            let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let json: Value = serde_json::from_slice(&body_bytes).unwrap();

            assert_eq!(json["error"], expected_message);
            assert_eq!(json["code"], expected_code);
            assert!(json["details"].is_string());
            assert_eq!(
                parts
                    .headers
                    .get(REQUEST_ID_HEADER)
                    .and_then(|value| value.to_str().ok()),
                json["request_id"].as_str()
            );
        }
    }

    #[tokio::test]
    async fn rate_limit_guidance_agrees_across_header_and_body() {
        let response = AppError::RateLimited {
            retry_after_seconds: 12,
        }
        .into_response();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(retry_after, "12");
        assert_eq!(json["retry_after_ms"], 12_000);
        assert_eq!(json["code"], "rate_limited");
    }

    #[tokio::test]
    async fn internal_errors_do_not_leak_diagnostics() {
        let secret = "Bearer should-never-escape";
        let response = AppError::Internal(anyhow::anyhow!(secret)).into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["code"], "internal_error");
        assert_eq!(json["details"], "Internal server error");
        assert!(!text.contains(secret));
    }

    #[test]
    fn test_database_error_conversion() {
        let sqlite_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed".to_string()),
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
        assert!(matches!(
            returns_error().unwrap_err(),
            AppError::Unauthorized
        ));
    }
}
