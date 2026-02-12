//! Data types for the OctoStore API.

use serde::{Deserialize, Serialize};

/// Lock status enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockStatus {
    /// Lock is available for acquisition.
    Free,
    /// Lock is currently held.
    Held,
}

/// Information about a lock.
#[derive(Debug, Clone, Deserialize)]
pub struct LockInfo {
    /// Name of the lock.
    pub name: String,
    /// Current status of the lock.
    pub status: LockStatus,
    /// ID of the current holder (if held).
    pub holder_id: Option<String>,
    /// Fencing token for coordination.
    pub fencing_token: u64,
    /// Expiration time (if held).
    pub expires_at: Option<String>,
}

/// Result of a lock acquisition attempt.
#[derive(Debug, Clone, Deserialize)]
pub struct AcquireResult {
    /// Status of the acquisition ("acquired" or "held").
    pub status: String,
    /// Lease ID for the acquired lock.
    pub lease_id: Option<String>,
    /// Fencing token for coordination.
    pub fencing_token: Option<u64>,
    /// Expiration time of the lock.
    pub expires_at: Option<String>,
    /// Current holder ID (if status is "held").
    pub holder_id: Option<String>,
}

/// Result of a lock renewal.
#[derive(Debug, Clone, Deserialize)]
pub struct RenewResult {
    /// Lease ID of the renewed lock.
    pub lease_id: String,
    /// New expiration time.
    pub expires_at: String,
}

/// Information about an authentication token.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenInfo {
    /// New authentication token.
    pub token: String,
    /// User ID.
    pub user_id: String,
    /// GitHub username.
    pub github_username: String,
}

/// A lock owned by the user.
#[derive(Debug, Clone, Deserialize)]
pub struct UserLock {
    /// Name of the lock.
    pub name: String,
    /// Lease ID of the lock.
    pub lease_id: String,
    /// Fencing token for coordination.
    pub fencing_token: u64,
    /// Expiration time.
    pub expires_at: String,
}

/// Request body for acquiring a lock.
#[derive(Debug, Serialize)]
pub struct AcquireRequest {
    /// Time-to-live in seconds.
    pub ttl_seconds: u32,
}

/// Request body for releasing a lock.
#[derive(Debug, Serialize)]
pub struct ReleaseRequest {
    /// Lease ID of the lock to release.
    pub lease_id: String,
}

/// Request body for renewing a lock.
#[derive(Debug, Serialize)]
pub struct RenewRequest {
    /// Lease ID of the lock to renew.
    pub lease_id: String,
    /// New time-to-live in seconds.
    pub ttl_seconds: u32,
}

/// Response body for listing locks.
#[derive(Debug, Deserialize)]
pub struct ListLocksResponse {
    /// List of user's locks.
    pub locks: Vec<UserLock>,
}