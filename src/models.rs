use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub github_id: u64,
    pub github_username: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

/// A held distributed lock with its ownership and expiry metadata.
#[derive(Debug, Clone, Serialize)]
pub struct Lock {
    pub name: String,
    pub holder_id: Uuid,
    pub lease_id: Uuid,
    pub fencing_token: u64,
    pub expires_at: DateTime<Utc>,
    pub metadata: Option<String>,
    pub acquired_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockEventType {
    Acquired,
    Released,
    Renewed,
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockEvent {
    pub event: LockEventType,
    pub lock_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock: Option<Lock>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct AcquireLockRequest {
    pub ttl_seconds: Option<u32>,
    pub metadata: Option<String>,
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum AcquireLockResponse {
    #[serde(rename = "acquired")]
    Acquired {
        lease_id: Uuid,
        fencing_token: u64,
        expires_at: DateTime<Utc>,
        metadata: Option<String>,
    },
    #[serde(rename = "held")]
    Held {
        holder_id: uuid::Uuid,
        expires_at: DateTime<Utc>,
        metadata: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub struct ReleaseLockRequest {
    pub lease_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct RenewLockRequest {
    pub lease_id: Uuid,
    pub ttl_seconds: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct RenewLockResponse {
    pub lease_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct LockStatusResponse {
    pub name: String,
    pub status: String, // "free" or "held"
    pub holder_id: Option<Uuid>,
    pub fencing_token: u64,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UserLocksResponse {
    pub locks: Vec<UserLockInfo>,
}

#[derive(Debug, Serialize)]
pub struct UserLockInfo {
    pub name: String,
    pub lease_id: Uuid,
    pub fencing_token: u64,
    pub expires_at: DateTime<Utc>,
    pub metadata: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthTokenResponse {
    pub token: String,
    pub user_id: Uuid,
    pub github_username: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GitHubUser {
    pub id: u64,
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct GitHubTokenResponse {
    pub access_token: String,
}

// ── Session models ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub ttl_seconds: u32,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub ttl_seconds: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub session_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub keepalive_interval_secs: u32,
}

#[derive(Debug, Serialize)]
pub struct KeepAliveResponse {
    pub session_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct SessionStatusResponse {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub lock_count: usize,
    pub active: bool,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

impl Lock {
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

/// Validates a lock name against length and character constraints.
///
/// Names must be 1–128 characters, containing only alphanumeric characters,
/// hyphens, and dots. This keeps lock names safe for use as database keys
/// and URL path segments.
pub fn validate_lock_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(AppError::InvalidLockName {
            reason: "Lock name cannot be empty".to_string(),
        });
    }

    if name.len() > 128 {
        return Err(AppError::InvalidLockName {
            reason: "Lock name cannot exceed 128 characters".to_string(),
        });
    }

    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.') {
        return Err(AppError::InvalidLockName {
            reason: "Lock name can only contain alphanumeric characters, hyphens, and dots"
                .to_string(),
        });
    }

    Ok(())
}

pub fn validate_ttl(ttl_seconds: u32) -> Result<()> {
    if ttl_seconds == 0 {
        return Err(AppError::InvalidTtl {
            reason: "TTL must be greater than 0".to_string(),
        });
    }

    if ttl_seconds > 3600 {
        return Err(AppError::InvalidTtl {
            reason: "TTL cannot exceed 3600 seconds (1 hour)".to_string(),
        });
    }

    Ok(())
}

pub fn validate_metadata(metadata: &Option<String>) -> Result<()> {
    if let Some(meta) = metadata {
        if meta.len() > 1024 {
            return Err(AppError::InvalidInput(
                "Metadata cannot exceed 1024 bytes".to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use chrono::{Duration, Utc};
    use serde_json;

    #[test]
    fn test_lock_is_expired() {
        let now = Utc::now();
        
        // Test expired lock
        let expired_lock = Lock {
            name: "test-lock".to_string(),
            holder_id: uuid::Uuid::new_v4(),
            lease_id: uuid::Uuid::new_v4(),
            fencing_token: 1,
            expires_at: now - Duration::minutes(1), // 1 minute ago
            metadata: None,
            acquired_at: now - Duration::minutes(5),
            session_id: None,
        };
        assert!(expired_lock.is_expired());

        // Test non-expired lock
        let active_lock = Lock {
            name: "test-lock".to_string(),
            holder_id: uuid::Uuid::new_v4(),
            lease_id: uuid::Uuid::new_v4(),
            fencing_token: 2,
            expires_at: now + Duration::minutes(5), // 5 minutes from now
            metadata: Some("test metadata".to_string()),
            acquired_at: now,
            session_id: None,
        };
        assert!(!active_lock.is_expired());

        // Test lock expiring right now (should be considered expired)
        let now_lock = Lock {
            name: "test-lock".to_string(),
            holder_id: uuid::Uuid::new_v4(),
            lease_id: uuid::Uuid::new_v4(),
            fencing_token: 3,
            expires_at: now,
            metadata: None,
            acquired_at: now - Duration::minutes(1),
            session_id: None,
        };
        // This might be flaky due to timing, but should generally be expired
        // since some time has passed since we created 'now'
        assert!(now_lock.is_expired());
    }

    #[test]
    fn test_validate_lock_name() {
        // Valid lock names
        assert!(validate_lock_name("valid-name").is_ok());
        assert!(validate_lock_name("valid.name").is_ok());
        assert!(validate_lock_name("valid123").is_ok());
        assert!(validate_lock_name("a").is_ok());
        assert!(validate_lock_name("123").is_ok());
        assert!(validate_lock_name("test-lock-with-dashes").is_ok());
        assert!(validate_lock_name("test.lock.with.dots").is_ok());

        // Invalid lock names - empty
        assert!(matches!(
            validate_lock_name("").unwrap_err(),
            AppError::InvalidLockName { reason } if reason == "Lock name cannot be empty"
        ));

        // Invalid lock names - too long
        let long_name = "a".repeat(129);
        assert!(matches!(
            validate_lock_name(&long_name).unwrap_err(),
            AppError::InvalidLockName { reason } if reason == "Lock name cannot exceed 128 characters"
        ));

        // Invalid lock names - invalid characters
        assert!(matches!(
            validate_lock_name("invalid name").unwrap_err(),
            AppError::InvalidLockName { .. }
        ));
        assert!(matches!(
            validate_lock_name("invalid_name").unwrap_err(),
            AppError::InvalidLockName { .. }
        ));
        assert!(matches!(
            validate_lock_name("invalid@name").unwrap_err(),
            AppError::InvalidLockName { .. }
        ));
        assert!(matches!(
            validate_lock_name("invalid/name").unwrap_err(),
            AppError::InvalidLockName { .. }
        ));
    }

    #[test]
    fn test_validate_ttl() {
        // Valid TTLs
        assert!(validate_ttl(1).is_ok());
        assert!(validate_ttl(60).is_ok());
        assert!(validate_ttl(3600).is_ok()); // 1 hour max

        // Invalid TTL - zero
        assert!(matches!(
            validate_ttl(0).unwrap_err(),
            AppError::InvalidTtl { reason } if reason == "TTL must be greater than 0"
        ));

        // Invalid TTL - too large
        assert!(matches!(
            validate_ttl(3601).unwrap_err(),
            AppError::InvalidTtl { reason } if reason == "TTL cannot exceed 3600 seconds (1 hour)"
        ));
        assert!(matches!(
            validate_ttl(7200).unwrap_err(),
            AppError::InvalidTtl { .. }
        ));
    }

    #[test]
    fn test_validate_metadata() {
        // Valid metadata
        assert!(validate_metadata(&None).is_ok());
        assert!(validate_metadata(&Some("".to_string())).is_ok());
        assert!(validate_metadata(&Some("short metadata".to_string())).is_ok());
        
        // Exactly 1024 bytes should be OK
        let max_metadata = "a".repeat(1024);
        assert!(validate_metadata(&Some(max_metadata)).is_ok());

        // Too long metadata
        let long_metadata = "a".repeat(1025);
        assert!(matches!(
            validate_metadata(&Some(long_metadata)).unwrap_err(),
            AppError::InvalidInput(msg) if msg == "Metadata cannot exceed 1024 bytes"
        ));
    }

    #[test]
    fn test_acquire_response_tags_correctly() {
        let acquired = AcquireLockResponse::Acquired {
            lease_id: uuid::Uuid::new_v4(),
            fencing_token: 42,
            expires_at: Utc::now(),
            metadata: Some("test".to_string()),
        };
        let json = serde_json::to_string(&acquired).unwrap();
        assert!(json.contains("\"status\":\"acquired\""),
            "acquired variant must serialize with status=acquired");

        let held = AcquireLockResponse::Held {
            holder_id: uuid::Uuid::new_v4(),
            expires_at: Utc::now(),
            metadata: None,
        };
        let json = serde_json::to_string(&held).unwrap();
        assert!(json.contains("\"status\":\"held\""),
            "held variant must serialize with status=held");
    }

    #[test]
    fn test_boundary_values() {
        assert!(validate_lock_name(&"a".repeat(128)).is_ok(), "128-char name should be valid");
        assert!(validate_lock_name(&"a".repeat(129)).is_err(), "129-char name should be rejected");
        assert!(validate_ttl(1).is_ok());
        assert!(validate_ttl(3600).is_ok());
        assert!(validate_ttl(3601).is_err());
        assert!(validate_metadata(&Some("a".repeat(1024))).is_ok());
        assert!(validate_metadata(&Some("a".repeat(1025))).is_err());
    }
}
