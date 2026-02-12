//! Error types for the OctoStore SDK.

use thiserror::Error;

/// Result type alias for OctoStore operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for OctoStore operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Authentication failed.
    #[error("Authentication failed: {message}")]
    Authentication { message: String },

    /// Network or HTTP error.
    #[error("Network error: {source}")]
    Network {
        #[from]
        source: reqwest::Error,
    },

    /// Lock operation failed.
    #[error("Lock error: {message}")]
    Lock { message: String, status_code: u16 },

    /// Lock is currently held by another process.
    #[error("Lock is held: {0}")]
    LockHeld(#[from] LockHeldError),

    /// Input validation failed.
    #[error("Validation error: {message}")]
    Validation { message: String },

    /// JSON parsing error.
    #[error("JSON error: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    /// URL parsing error.
    #[error("URL error: {source}")]
    Url {
        #[from]
        source: url::ParseError,
    },

    /// Other error.
    #[error("Other error: {message}")]
    Other { message: String },
}

/// Error returned when trying to acquire a lock that's already held.
#[derive(Debug, Error)]
#[error("Lock '{lock_name}' is already held")]
pub struct LockHeldError {
    /// Name of the lock.
    pub lock_name: String,
    /// ID of the current holder.
    pub holder_id: Option<String>,
    /// Expiration time of the current lease.
    pub expires_at: Option<String>,
}

impl LockHeldError {
    /// Create a new LockHeldError.
    pub fn new(
        lock_name: String,
        holder_id: Option<String>,
        expires_at: Option<String>,
    ) -> Self {
        Self {
            lock_name,
            holder_id,
            expires_at,
        }
    }
}

impl Error {
    /// Create an authentication error.
    pub fn authentication(message: impl Into<String>) -> Self {
        Self::Authentication {
            message: message.into(),
        }
    }

    /// Create a lock error.
    pub fn lock(message: impl Into<String>, status_code: u16) -> Self {
        Self::Lock {
            message: message.into(),
            status_code,
        }
    }

    /// Create a validation error.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
        }
    }

    /// Create an other error.
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other {
            message: message.into(),
        }
    }
}