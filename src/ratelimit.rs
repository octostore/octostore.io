use crate::{
    auth::AuthService,
    error::{AppError, Result},
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tracing::debug;
use uuid::Uuid;

#[derive(Clone)]
pub struct RateLimitHandlers {
    pub store: Arc<RateLimitStore>,
    pub auth: AuthService,
}

impl RateLimitHandlers {
    pub fn new(auth: AuthService) -> Self {
        let store = Arc::new(RateLimitStore::new());
        let cleanup_store = store.clone();
        
        // Start background cleanup task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                cleanup_store.cleanup_expired().await;
            }
        });
        
        Self { store, auth }
    }
}

#[derive(Debug, Clone)]
pub struct RateLimitStore {
    limits: Arc<DashMap<(Uuid, String), RateLimitWindow>>,
}

#[derive(Debug, Clone)]
struct RateLimitWindow {
    window_start: DateTime<Utc>,
    count: u32,
    max_requests: u32,
    window_seconds: u32,
}

impl RateLimitStore {
    pub fn new() -> Self {
        Self {
            limits: Arc::new(DashMap::new()),
        }
    }

    pub async fn cleanup_expired(&self) {
        let now = Utc::now();
        let mut expired_keys = Vec::new();
        
        for entry in self.limits.iter() {
            let (key, window) = (entry.key().clone(), entry.value().clone());
            let window_end = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
            
            if now > window_end {
                expired_keys.push(key);
            }
        }
        
        for key in expired_keys {
            self.limits.remove(&key);
        }
        
        debug!("Cleaned up {} expired rate limit windows", self.limits.len());
    }

    pub fn check_rate_limit(&self, user_id: Uuid, limit_name: &str, max_requests: u32, window_seconds: u32) -> RateLimitResult {
        let key = (user_id, limit_name.to_string());
        let now = Utc::now();
        
        let mut entry = self.limits.entry(key).or_insert_with(|| RateLimitWindow {
            window_start: now,
            count: 0,
            max_requests,
            window_seconds,
        });
        
        let window = entry.value_mut();
        let window_end = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
        
        // Check if current window has expired, reset if so
        if now > window_end {
            window.window_start = now;
            window.count = 0;
            window.max_requests = max_requests;
            window.window_seconds = window_seconds;
        }
        
        // Check if request is allowed
        if window.count >= window.max_requests {
            let reset_at = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
            let retry_after_seconds = (reset_at - now).num_seconds().max(0) as u32;
            
            RateLimitResult {
                allowed: false,
                remaining: 0,
                reset_at,
                retry_after_seconds: Some(retry_after_seconds),
            }
        } else {
            // Consume the request
            window.count += 1;
            let remaining = window.max_requests - window.count;
            let reset_at = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
            
            RateLimitResult {
                allowed: true,
                remaining,
                reset_at,
                retry_after_seconds: None,
            }
        }
    }

    pub fn get_rate_limit_status(&self, user_id: Uuid, limit_name: &str) -> Option<RateLimitResult> {
        let key = (user_id, limit_name.to_string());
        let now = Utc::now();
        
        if let Some(entry) = self.limits.get(&key) {
            let window = entry.value();
            let window_end = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
            
            // Check if window has expired
            if now > window_end {
                return None;
            }
            
            let remaining = window.max_requests.saturating_sub(window.count);
            let reset_at = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
            let allowed = window.count < window.max_requests;
            let retry_after_seconds = if !allowed {
                Some((reset_at - now).num_seconds().max(0) as u32)
            } else {
                None
            };
            
            Some(RateLimitResult {
                allowed,
                remaining,
                reset_at,
                retry_after_seconds,
            })
        } else {
            None
        }
    }

    pub fn reset_rate_limit(&self, user_id: Uuid, limit_name: &str) -> bool {
        let key = (user_id, limit_name.to_string());
        self.limits.remove(&key).is_some()
    }

    pub fn list_user_rate_limits(&self, user_id: Uuid) -> Vec<UserRateLimit> {
        let now = Utc::now();
        let mut limits = Vec::new();
        
        for entry in self.limits.iter() {
            let ((entry_user_id, limit_name), window) = (entry.key(), entry.value());
            
            if *entry_user_id == user_id {
                let window_end = window.window_start + chrono::Duration::seconds(window.window_seconds as i64);
                
                // Skip expired windows
                if now <= window_end {
                    let remaining = window.max_requests.saturating_sub(window.count);
                    let allowed = window.count < window.max_requests;
                    
                    limits.push(UserRateLimit {
                        name: limit_name.clone(),
                        max_requests: window.max_requests,
                        window_seconds: window.window_seconds,
                        current_count: window.count,
                        remaining,
                        allowed,
                        reset_at: window_end,
                    });
                }
            }
        }
        
        limits
    }
}

#[derive(Debug, Serialize)]
pub struct RateLimitResult {
    pub allowed: bool,
    pub remaining: u32,
    pub reset_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct UserRateLimit {
    pub name: String,
    pub max_requests: u32,
    pub window_seconds: u32,
    pub current_count: u32,
    pub remaining: u32,
    pub allowed: bool,
    pub reset_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CheckRateLimitRequest {
    pub max_requests: u32,
    pub window_seconds: u32,
}

#[derive(Debug, Serialize)]
pub struct ListRateLimitsResponse {
    pub limits: Vec<UserRateLimit>,
}

// Route handlers

pub async fn check_rate_limit(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<CheckRateLimitRequest>,
) -> Result<Json<RateLimitResult>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    // Validate inputs
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::InvalidInput("Rate limit name must be 1-100 characters".to_string()));
    }
    
    if req.max_requests == 0 || req.max_requests > 10000 {
        return Err(AppError::InvalidInput("max_requests must be 1-10000".to_string()));
    }
    
    if req.window_seconds == 0 || req.window_seconds > 86400 {
        return Err(AppError::InvalidInput("window_seconds must be 1-86400 (24 hours)".to_string()));
    }
    
    let result = state.ratelimit_handlers.store.check_rate_limit(
        user_id, 
        &name, 
        req.max_requests, 
        req.window_seconds
    );
    
    Ok(Json(result))
}

pub async fn get_rate_limit_status(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<RateLimitResult>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    if let Some(result) = state.ratelimit_handlers.store.get_rate_limit_status(user_id, &name) {
        Ok(Json(result))
    } else {
        Err(AppError::NotFound("Rate limit not found".to_string()))
    }
}

pub async fn reset_rate_limit(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let reset = state.ratelimit_handlers.store.reset_rate_limit(user_id, &name);
    
    if reset {
        Ok(Json(serde_json::json!({"reset": true})))
    } else {
        Err(AppError::NotFound("Rate limit not found".to_string()))
    }
}

pub async fn list_rate_limits(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<ListRateLimitsResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let limits = state.ratelimit_handlers.store.list_user_rate_limits(user_id);
    
    Ok(Json(ListRateLimitsResponse { limits }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use tokio::time::{sleep, Duration as TokioDuration};

    #[test]
    fn test_rate_limit_store_new() {
        let store = RateLimitStore::new();
        assert_eq!(store.limits.len(), 0);
    }

    #[test]
    fn test_check_rate_limit_allows_within_limit() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // First request should be allowed
        let result = store.check_rate_limit(user_id, limit_name, 5, 60);
        assert!(result.allowed);
        assert_eq!(result.remaining, 4);
        assert!(result.retry_after_seconds.is_none());
        
        // Second request should also be allowed
        let result = store.check_rate_limit(user_id, limit_name, 5, 60);
        assert!(result.allowed);
        assert_eq!(result.remaining, 3);
    }

    #[test]
    fn test_check_rate_limit_blocks_over_limit() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // Use up all requests
        for i in 0..3 {
            let result = store.check_rate_limit(user_id, limit_name, 3, 60);
            assert!(result.allowed);
            assert_eq!(result.remaining, 2 - i);
        }
        
        // Next request should be blocked
        let result = store.check_rate_limit(user_id, limit_name, 3, 60);
        assert!(!result.allowed);
        assert_eq!(result.remaining, 0);
        assert!(result.retry_after_seconds.is_some());
        assert!(result.retry_after_seconds.unwrap() <= 60);
    }

    #[test]
    fn test_check_rate_limit_per_key_isolation() {
        let store = RateLimitStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // User 1 uses up their limit
        for _ in 0..3 {
            let result = store.check_rate_limit(user1, limit_name, 3, 60);
            assert!(result.allowed);
        }
        
        // User 1 is blocked
        let result = store.check_rate_limit(user1, limit_name, 3, 60);
        assert!(!result.allowed);
        
        // User 2 should still be allowed (different key)
        let result = store.check_rate_limit(user2, limit_name, 3, 60);
        assert!(result.allowed);
        assert_eq!(result.remaining, 2);
        
        // Same user, different limit name should also be allowed
        let result = store.check_rate_limit(user1, "different_limit", 3, 60);
        assert!(result.allowed);
        assert_eq!(result.remaining, 2);
    }

    #[test]
    fn test_rate_limit_window_expiry() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // Create a rate limit window manually with past window_start
        let past_time = Utc::now() - Duration::seconds(120); // 2 minutes ago
        let window = RateLimitWindow {
            window_start: past_time,
            count: 5,
            max_requests: 3,
            window_seconds: 60, // 1 minute window
        };
        let key = (user_id, limit_name.to_string());
        store.limits.insert(key, window);
        
        // Should create a new window since the old one expired
        let result = store.check_rate_limit(user_id, limit_name, 3, 60);
        assert!(result.allowed);
        assert_eq!(result.remaining, 2); // Should reset to new limit
    }

    #[test]
    fn test_get_rate_limit_status_existing() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // Create some usage
        store.check_rate_limit(user_id, limit_name, 5, 60);
        store.check_rate_limit(user_id, limit_name, 5, 60);
        
        // Check status
        let status = store.get_rate_limit_status(user_id, limit_name);
        assert!(status.is_some());
        let status = status.unwrap();
        assert!(status.allowed);
        assert_eq!(status.remaining, 3);
    }

    #[test]
    fn test_get_rate_limit_status_nonexistent() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "nonexistent";
        
        let status = store.get_rate_limit_status(user_id, limit_name);
        assert!(status.is_none());
    }

    #[test]
    fn test_get_rate_limit_status_expired() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // Create expired window manually
        let past_time = Utc::now() - Duration::seconds(120);
        let window = RateLimitWindow {
            window_start: past_time,
            count: 2,
            max_requests: 5,
            window_seconds: 60,
        };
        let key = (user_id, limit_name.to_string());
        store.limits.insert(key, window);
        
        // Should return None since window is expired
        let status = store.get_rate_limit_status(user_id, limit_name);
        assert!(status.is_none());
    }

    #[test]
    fn test_reset_rate_limit() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "test_limit";
        
        // Create some usage
        store.check_rate_limit(user_id, limit_name, 5, 60);
        
        // Reset should return true and remove the entry
        let reset = store.reset_rate_limit(user_id, limit_name);
        assert!(reset);
        
        // Should return None after reset
        let status = store.get_rate_limit_status(user_id, limit_name);
        assert!(status.is_none());
        
        // Resetting non-existent limit should return false
        let reset = store.reset_rate_limit(user_id, "nonexistent");
        assert!(!reset);
    }

    #[test]
    fn test_list_user_rate_limits() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let other_user_id = Uuid::new_v4();
        
        // Create limits for user
        store.check_rate_limit(user_id, "limit1", 5, 60);
        store.check_rate_limit(user_id, "limit1", 5, 60); // Use 2/5
        store.check_rate_limit(user_id, "limit2", 3, 120);
        
        // Create limit for different user (should not appear)
        store.check_rate_limit(other_user_id, "limit3", 10, 300);
        
        let limits = store.list_user_rate_limits(user_id);
        assert_eq!(limits.len(), 2);
        
        // Find limit1
        let limit1 = limits.iter().find(|l| l.name == "limit1").unwrap();
        assert_eq!(limit1.max_requests, 5);
        assert_eq!(limit1.window_seconds, 60);
        assert_eq!(limit1.current_count, 2);
        assert_eq!(limit1.remaining, 3);
        assert!(limit1.allowed);
        
        // Find limit2
        let limit2 = limits.iter().find(|l| l.name == "limit2").unwrap();
        assert_eq!(limit2.max_requests, 3);
        assert_eq!(limit2.window_seconds, 120);
        assert_eq!(limit2.current_count, 1);
        assert_eq!(limit2.remaining, 2);
        assert!(limit2.allowed);
    }

    #[test]
    fn test_list_user_rate_limits_excludes_expired() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        
        // Create current limit
        store.check_rate_limit(user_id, "current", 5, 60);
        
        // Create expired limit manually
        let past_time = Utc::now() - Duration::seconds(120);
        let expired_window = RateLimitWindow {
            window_start: past_time,
            count: 3,
            max_requests: 5,
            window_seconds: 60,
        };
        let expired_key = (user_id, "expired".to_string());
        store.limits.insert(expired_key, expired_window);
        
        let limits = store.list_user_rate_limits(user_id);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].name, "current");
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        
        // Create current limit
        store.check_rate_limit(user_id, "current", 5, 60);
        
        // Create expired limit manually
        let past_time = Utc::now() - Duration::seconds(120);
        let expired_window = RateLimitWindow {
            window_start: past_time,
            count: 3,
            max_requests: 5,
            window_seconds: 60,
        };
        let expired_key = (user_id, "expired".to_string());
        store.limits.insert(expired_key, expired_window);
        
        assert_eq!(store.limits.len(), 2);
        
        // Run cleanup
        store.cleanup_expired().await;
        
        // Should only have the current limit left
        assert_eq!(store.limits.len(), 1);
        let remaining = store.limits.iter().next().unwrap();
        assert_eq!(remaining.key().1, "current");
    }

    #[test]
    fn test_rate_limit_result_serialization() {
        let result = RateLimitResult {
            allowed: true,
            remaining: 5,
            reset_at: Utc::now(),
            retry_after_seconds: None,
        };
        
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"allowed\":true"));
        assert!(json.contains("\"remaining\":5"));
        assert!(!json.contains("retry_after_seconds")); // Should be omitted when None
        
        let blocked_result = RateLimitResult {
            allowed: false,
            remaining: 0,
            reset_at: Utc::now(),
            retry_after_seconds: Some(30),
        };
        
        let json = serde_json::to_string(&blocked_result).unwrap();
        assert!(json.contains("\"allowed\":false"));
        assert!(json.contains("\"retry_after_seconds\":30"));
    }

    #[test]
    fn test_check_rate_limit_request_deserialization() {
        let json = r#"{"max_requests":100,"window_seconds":3600}"#;
        let req: CheckRateLimitRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.max_requests, 100);
        assert_eq!(req.window_seconds, 3600);
    }

    #[test]
    fn test_user_rate_limit_serialization() {
        let limit = UserRateLimit {
            name: "test_limit".to_string(),
            max_requests: 100,
            window_seconds: 3600,
            current_count: 25,
            remaining: 75,
            allowed: true,
            reset_at: Utc::now(),
        };
        
        let json = serde_json::to_string(&limit).unwrap();
        assert!(json.contains("\"name\":\"test_limit\""));
        assert!(json.contains("\"max_requests\":100"));
        assert!(json.contains("\"window_seconds\":3600"));
        assert!(json.contains("\"current_count\":25"));
        assert!(json.contains("\"remaining\":75"));
        assert!(json.contains("\"allowed\":true"));
    }

    #[test]
    fn test_rate_limit_edge_cases() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        
        // Test with limit of 1
        let result = store.check_rate_limit(user_id, "edge_case_1", 1, 60);
        assert!(result.allowed);
        assert_eq!(result.remaining, 0);
        
        let result = store.check_rate_limit(user_id, "edge_case_1", 1, 60);
        assert!(!result.allowed);
        assert_eq!(result.remaining, 0);
        
        // Test with very large window
        let result = store.check_rate_limit(user_id, "edge_case_2", 1000, 86400);
        assert!(result.allowed);
        assert_eq!(result.remaining, 999);
    }

    #[test]
    fn test_rate_limit_window_boundary() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "boundary_test";
        
        // Create a window that's about to expire (1 second window)
        store.check_rate_limit(user_id, limit_name, 1, 1);
        
        // Should be blocked immediately
        let result = store.check_rate_limit(user_id, limit_name, 1, 1);
        assert!(!result.allowed);
        
        // Manually expire the window by setting past time
        let past_time = Utc::now() - Duration::seconds(2);
        let expired_window = RateLimitWindow {
            window_start: past_time,
            count: 1,
            max_requests: 1,
            window_seconds: 1,
        };
        let key = (user_id, limit_name.to_string());
        store.limits.insert(key, expired_window);
        
        // Should be allowed again after window expires
        let result = store.check_rate_limit(user_id, limit_name, 1, 1);
        assert!(result.allowed);
    }

    #[test]
    fn test_saturating_sub_in_remaining_calculation() {
        let store = RateLimitStore::new();
        let user_id = Uuid::new_v4();
        let limit_name = "saturation_test";
        
        // Manually create a window where count exceeds max_requests
        // (This shouldn't happen in normal operation but tests edge case)
        let window = RateLimitWindow {
            window_start: Utc::now(),
            count: 10,
            max_requests: 5,
            window_seconds: 60,
        };
        let key = (user_id, limit_name.to_string());
        store.limits.insert(key, window);
        
        let status = store.get_rate_limit_status(user_id, limit_name);
        assert!(status.is_some());
        let status = status.unwrap();
        assert_eq!(status.remaining, 0); // Should saturate to 0, not underflow
        assert!(!status.allowed);
    }
}