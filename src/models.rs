use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub github_id: u64,
    pub github_username: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Lock {
    pub name: String,
    pub holder_id: Uuid,
    pub lease_id: Uuid,
    pub fencing_token: u64,
    pub expires_at: DateTime<Utc>,
    pub metadata: Option<String>,
    pub acquired_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct AcquireLockRequest {
    pub ttl_seconds: Option<u32>,
    pub metadata: Option<String>,
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
        holder_id: Uuid,
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

#[derive(Debug, Deserialize)]
pub struct GitHubUser {
    pub id: u64,
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct GitHubTokenResponse {
    pub access_token: String,
}

impl Lock {
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

pub fn validate_lock_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Lock name cannot be empty".to_string());
    }
    
    if name.len() > 128 {
        return Err("Lock name cannot exceed 128 characters".to_string());
    }
    
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.') {
        return Err("Lock name can only contain alphanumeric characters, hyphens, and dots".to_string());
    }
    
    Ok(())
}

pub fn validate_ttl(ttl_seconds: u32) -> Result<(), String> {
    if ttl_seconds == 0 {
        return Err("TTL must be greater than 0".to_string());
    }
    
    if ttl_seconds > 3600 {
        return Err("TTL cannot exceed 3600 seconds (1 hour)".to_string());
    }
    
    Ok(())
}

pub fn validate_metadata(metadata: &Option<String>) -> Result<(), String> {
    if let Some(meta) = metadata {
        if meta.len() > 1024 {
            return Err("Metadata cannot exceed 1024 bytes".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(
            validate_lock_name("").unwrap_err(),
            "Lock name cannot be empty"
        );

        // Invalid lock names - too long
        let long_name = "a".repeat(129);
        assert_eq!(
            validate_lock_name(&long_name).unwrap_err(),
            "Lock name cannot exceed 128 characters"
        );

        // Invalid lock names - invalid characters
        assert_eq!(
            validate_lock_name("invalid name").unwrap_err(),
            "Lock name can only contain alphanumeric characters, hyphens, and dots"
        );
        assert_eq!(
            validate_lock_name("invalid_name").unwrap_err(),
            "Lock name can only contain alphanumeric characters, hyphens, and dots"
        );
        assert_eq!(
            validate_lock_name("invalid@name").unwrap_err(),
            "Lock name can only contain alphanumeric characters, hyphens, and dots"
        );
        assert_eq!(
            validate_lock_name("invalid/name").unwrap_err(),
            "Lock name can only contain alphanumeric characters, hyphens, and dots"
        );
    }

    #[test]
    fn test_validate_ttl() {
        // Valid TTLs
        assert!(validate_ttl(1).is_ok());
        assert!(validate_ttl(60).is_ok());
        assert!(validate_ttl(3600).is_ok()); // 1 hour max

        // Invalid TTL - zero
        assert_eq!(
            validate_ttl(0).unwrap_err(),
            "TTL must be greater than 0"
        );

        // Invalid TTL - too large
        assert_eq!(
            validate_ttl(3601).unwrap_err(),
            "TTL cannot exceed 3600 seconds (1 hour)"
        );
        assert_eq!(
            validate_ttl(7200).unwrap_err(),
            "TTL cannot exceed 3600 seconds (1 hour)"
        );
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
        assert_eq!(
            validate_metadata(&Some(long_metadata)).unwrap_err(),
            "Metadata cannot exceed 1024 bytes"
        );
    }

    #[test]
    fn test_user_serialization() {
        let user = User {
            id: uuid::Uuid::new_v4(),
            github_id: 12345,
            github_username: "testuser".to_string(),
            token: "test-token".to_string(),
            created_at: Utc::now(),
        };

        // Test serialization
        let json = serde_json::to_string(&user).unwrap();
        assert!(json.contains("\"github_id\":12345"));
        assert!(json.contains("\"github_username\":\"testuser\""));

        // Test deserialization
        let deserialized: User = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, user.id);
        assert_eq!(deserialized.github_id, user.github_id);
        assert_eq!(deserialized.github_username, user.github_username);
        assert_eq!(deserialized.token, user.token);
    }

    #[test]
    fn test_acquire_lock_response_serialization() {
        let lease_id = uuid::Uuid::new_v4();
        let expires_at = Utc::now();
        
        // Test Acquired response
        let acquired = AcquireLockResponse::Acquired {
            lease_id,
            fencing_token: 42,
            expires_at,
            metadata: Some("test".to_string()),
        };
        
        let json = serde_json::to_string(&acquired).unwrap();
        assert!(json.contains("\"status\":\"acquired\""));
        assert!(json.contains("\"fencing_token\":42"));
        assert!(json.contains("\"metadata\":\"test\""));

        // Test Held response
        let holder_id = uuid::Uuid::new_v4();
        let held = AcquireLockResponse::Held {
            holder_id,
            expires_at,
            metadata: None,
        };
        
        let json = serde_json::to_string(&held).unwrap();
        assert!(json.contains("\"status\":\"held\""));
        assert!(json.contains(&format!("\"holder_id\":\"{}\"", holder_id)));
        assert!(json.contains("\"metadata\":null"));
    }

    #[test]
    fn test_request_deserialization() {
        // Test AcquireLockRequest
        let json = r#"{"ttl_seconds":300,"metadata":"test metadata"}"#;
        let request: AcquireLockRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.ttl_seconds, Some(300));
        assert_eq!(request.metadata, Some("test metadata".to_string()));

        // Test with nulls
        let json = r#"{"ttl_seconds":null,"metadata":null}"#;
        let request: AcquireLockRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.ttl_seconds, None);
        assert_eq!(request.metadata, None);

        // Test ReleaseLockRequest
        let lease_id = uuid::Uuid::new_v4();
        let json = format!(r#"{{"lease_id":"{}"}}"#, lease_id);
        let request: ReleaseLockRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(request.lease_id, lease_id);

        // Test RenewLockRequest
        let json = format!(r#"{{"lease_id":"{}","ttl_seconds":600}}"#, lease_id);
        let request: RenewLockRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(request.lease_id, lease_id);
        assert_eq!(request.ttl_seconds, Some(600));
    }

    #[test]
    fn test_github_models() {
        // Test GitHubUser
        let json = r#"{"id":12345,"login":"testuser"}"#;
        let user: GitHubUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.id, 12345);
        assert_eq!(user.login, "testuser");

        // Test GitHubTokenResponse
        let json = r#"{"access_token":"gho_1234567890abcdef"}"#;
        let token: GitHubTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(token.access_token, "gho_1234567890abcdef");
    }

    #[test]
    fn test_lock_clone() {
        let lock = Lock {
            name: "test-lock".to_string(),
            holder_id: uuid::Uuid::new_v4(),
            lease_id: uuid::Uuid::new_v4(),
            fencing_token: 1,
            expires_at: Utc::now(),
            metadata: Some("test".to_string()),
            acquired_at: Utc::now(),
        };

        let cloned = lock.clone();
        assert_eq!(lock.name, cloned.name);
        assert_eq!(lock.holder_id, cloned.holder_id);
        assert_eq!(lock.lease_id, cloned.lease_id);
        assert_eq!(lock.fencing_token, cloned.fencing_token);
        assert_eq!(lock.metadata, cloned.metadata);
    }

    #[test]
    fn test_edge_cases() {
        // Test lock name with exactly 128 characters
        let max_name = "a".repeat(128);
        assert!(validate_lock_name(&max_name).is_ok());

        // Test boundary TTL values
        assert!(validate_ttl(1).is_ok());     // minimum
        assert!(validate_ttl(3600).is_ok());  // maximum
    }
}