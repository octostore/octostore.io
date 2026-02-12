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