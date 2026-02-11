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
}

#[derive(Debug, Deserialize)]
pub struct AcquireLockRequest {
    pub ttl_seconds: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum AcquireLockResponse {
    #[serde(rename = "acquired")]
    Acquired {
        lease_id: Uuid,
        fencing_token: u64,
        expires_at: DateTime<Utc>,
    },
    #[serde(rename = "held")]
    Held {
        holder_id: Uuid,
        expires_at: DateTime<Utc>,
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