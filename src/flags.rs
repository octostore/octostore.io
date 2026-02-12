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
use std::{collections::hash_map::DefaultHasher, hash::{Hash, Hasher}, sync::Arc};
use uuid::Uuid;

#[derive(Clone)]
pub struct FeatureFlagHandlers {
    pub store: Arc<FeatureFlagStore>,
    pub auth: AuthService,
}

impl FeatureFlagHandlers {
    pub fn new(auth: AuthService) -> Self {
        let store = Arc::new(FeatureFlagStore::new());
        Self { store, auth }
    }
}

#[derive(Debug, Clone)]
pub struct FeatureFlagStore {
    flags: Arc<DashMap<(Uuid, String), FeatureFlag>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureFlag {
    pub name: String,
    pub enabled: bool,
    pub percentage: Option<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl FeatureFlagStore {
    pub fn new() -> Self {
        Self {
            flags: Arc::new(DashMap::new()),
        }
    }

    pub fn set_flag(&self, user_id: Uuid, flag_name: &str, enabled: bool, percentage: Option<u8>) -> Result<FeatureFlag> {
        // Check user flag limit (max 1000)
        let user_flag_count = self.flags.iter()
            .filter(|entry| entry.key().0 == user_id)
            .count();
        
        if user_flag_count >= 1000 {
            return Err(AppError::Conflict("Maximum number of flags (1000) reached".to_string()));
        }
        
        // Validate percentage if provided
        if let Some(pct) = percentage {
            if pct > 100 {
                return Err(AppError::InvalidInput("Percentage must be 0-100".to_string()));
            }
        }
        
        let key = (user_id, flag_name.to_string());
        let now = Utc::now();
        
        let flag = match self.flags.get(&key) {
            Some(existing) => {
                let mut updated_flag = existing.value().clone();
                updated_flag.enabled = enabled;
                updated_flag.percentage = percentage;
                updated_flag.updated_at = now;
                updated_flag
            }
            None => FeatureFlag {
                name: flag_name.to_string(),
                enabled,
                percentage,
                created_at: now,
                updated_at: now,
            }
        };
        
        self.flags.insert(key, flag.clone());
        Ok(flag)
    }

    pub fn get_flag(&self, user_id: Uuid, flag_name: &str) -> Option<FlagEvaluation> {
        let key = (user_id, flag_name.to_string());
        
        if let Some(entry) = self.flags.get(&key) {
            let flag = entry.value();
            
            let enabled = if flag.enabled {
                if let Some(percentage) = flag.percentage {
                    // Use consistent hashing for percentage rollout
                    let evaluation_key = format!("{}-{}", flag_name, user_id);
                    let mut hasher = DefaultHasher::new();
                    evaluation_key.hash(&mut hasher);
                    let hash = hasher.finish();
                    let hash_percentage = (hash % 100) as u8;
                    hash_percentage < percentage
                } else {
                    true
                }
            } else {
                false
            };
            
            Some(FlagEvaluation {
                name: flag.name.clone(),
                enabled,
            })
        } else {
            None
        }
    }

    pub fn delete_flag(&self, user_id: Uuid, flag_name: &str) -> bool {
        let key = (user_id, flag_name.to_string());
        self.flags.remove(&key).is_some()
    }

    pub fn list_user_flags(&self, user_id: Uuid) -> Vec<UserFeatureFlag> {
        let mut flags = Vec::new();
        
        for entry in self.flags.iter() {
            let ((entry_user_id, _), flag) = (entry.key(), entry.value());
            
            if *entry_user_id == user_id {
                flags.push(UserFeatureFlag {
                    name: flag.name.clone(),
                    enabled: flag.enabled,
                    percentage: flag.percentage,
                    created_at: flag.created_at,
                    updated_at: flag.updated_at,
                });
            }
        }
        
        // Sort by name for consistent ordering
        flags.sort_by(|a, b| a.name.cmp(&b.name));
        flags
    }
}

#[derive(Debug, Serialize)]
pub struct FlagEvaluation {
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct UserFeatureFlag {
    pub name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct SetFlagRequest {
    pub enabled: bool,
    #[serde(default)]
    pub percentage: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct ListFlagsResponse {
    pub flags: Vec<UserFeatureFlag>,
}

// Route handlers

pub async fn set_flag(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<SetFlagRequest>,
) -> Result<Json<UserFeatureFlag>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    // Validate flag name
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::InvalidInput("Flag name must be 1-100 characters".to_string()));
    }
    
    // Flag name should be alphanumeric, hyphens, underscores only
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err(AppError::InvalidInput("Flag name can only contain letters, numbers, hyphens, and underscores".to_string()));
    }
    
    let flag = state.flags_handlers.store.set_flag(
        user_id,
        &name,
        req.enabled,
        req.percentage,
    )?;
    
    let response = UserFeatureFlag {
        name: flag.name,
        enabled: flag.enabled,
        percentage: flag.percentage,
        created_at: flag.created_at,
        updated_at: flag.updated_at,
    };
    
    Ok(Json(response))
}

pub async fn get_flag(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<FlagEvaluation>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    if let Some(evaluation) = state.flags_handlers.store.get_flag(user_id, &name) {
        Ok(Json(evaluation))
    } else {
        // Return default false for non-existent flags
        Ok(Json(FlagEvaluation {
            name: name.clone(),
            enabled: false,
        }))
    }
}

pub async fn delete_flag(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let deleted = state.flags_handlers.store.delete_flag(user_id, &name);
    
    if deleted {
        Ok(Json(serde_json::json!({"deleted": true})))
    } else {
        Err(AppError::NotFound("Flag not found".to_string()))
    }
}

pub async fn list_flags(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<ListFlagsResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let flags = state.flags_handlers.store.list_user_flags(user_id);
    
    Ok(Json(ListFlagsResponse { flags }))
}