use crate::{
    auth::AuthService,
    error::{AppError, Result},
};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc};
use uuid::Uuid;

#[derive(Clone)]
pub struct ConfigStoreHandlers {
    pub store: Arc<ConfigStore>,
    pub auth: AuthService,
}

impl ConfigStoreHandlers {
    pub fn new(auth: AuthService) -> Self {
        let store = Arc::new(ConfigStore::new());
        Self { store, auth }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    configs: Arc<DashMap<(Uuid, String), ConfigEntry>>,
    history: Arc<DashMap<(Uuid, String), VecDeque<ConfigVersion>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigEntry {
    pub key: String,
    pub value: serde_json::Value,
    pub description: Option<String>,
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigVersion {
    pub version: u32,
    pub value: serde_json::Value,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl ConfigStore {
    pub fn new() -> Self {
        Self {
            configs: Arc::new(DashMap::new()),
            history: Arc::new(DashMap::new()),
        }
    }

    pub fn set_config(&self, user_id: Uuid, config_key: &str, value: serde_json::Value, description: Option<String>) -> Result<ConfigEntry> {
        // Check user config limit (max 500)
        let user_config_count = self.configs.iter()
            .filter(|entry| entry.key().0 == user_id)
            .count();
        
        let key = (user_id, config_key.to_string());
        let is_new = !self.configs.contains_key(&key);
        
        if is_new && user_config_count >= 500 {
            return Err(AppError::Conflict("Maximum number of config keys (500) reached".to_string()));
        }
        
        let now = Utc::now();
        
        // Get or create config entry
        let config = match self.configs.get(&key) {
            Some(existing) => {
                let mut updated_config = existing.value().clone();
                
                // Store current version in history before updating
                let history_entry = ConfigVersion {
                    version: updated_config.version,
                    value: updated_config.value.clone(),
                    description: updated_config.description.clone(),
                    created_at: updated_config.updated_at,
                };
                
                let mut history_queue = self.history.entry(key.clone()).or_insert_with(VecDeque::new);
                history_queue.push_back(history_entry);
                
                // Keep only last 10 versions
                if history_queue.len() > 10 {
                    history_queue.pop_front();
                }
                
                // Update current config
                updated_config.value = value;
                updated_config.description = description;
                updated_config.version = updated_config.version + 1;
                updated_config.updated_at = now;
                updated_config
            }
            None => {
                ConfigEntry {
                    key: config_key.to_string(),
                    value,
                    description,
                    version: 1,
                    created_at: now,
                    updated_at: now,
                }
            }
        };
        
        self.configs.insert(key, config.clone());
        Ok(config)
    }

    pub fn get_config(&self, user_id: Uuid, config_key: &str, version: Option<u32>) -> Option<ConfigEntry> {
        let key = (user_id, config_key.to_string());
        
        match version {
            Some(requested_version) => {
                // Get specific version
                if let Some(current) = self.configs.get(&key) {
                    if current.version == requested_version {
                        return Some(current.value().clone());
                    }
                }
                
                // Check history for the requested version
                if let Some(history) = self.history.get(&key) {
                    for version_entry in history.iter() {
                        if version_entry.version == requested_version {
                            return Some(ConfigEntry {
                                key: config_key.to_string(),
                                value: version_entry.value.clone(),
                                description: version_entry.description.clone(),
                                version: version_entry.version,
                                created_at: version_entry.created_at,
                                updated_at: version_entry.created_at,
                            });
                        }
                    }
                }
                None
            }
            None => {
                // Get current version
                self.configs.get(&key).map(|entry| entry.value().clone())
            }
        }
    }

    pub fn get_config_history(&self, user_id: Uuid, config_key: &str) -> Vec<ConfigVersion> {
        let key = (user_id, config_key.to_string());
        let mut versions = Vec::new();
        
        // Add current version
        if let Some(current) = self.configs.get(&key) {
            versions.push(ConfigVersion {
                version: current.version,
                value: current.value.clone(),
                description: current.description.clone(),
                created_at: current.updated_at,
            });
        }
        
        // Add historical versions
        if let Some(history) = self.history.get(&key) {
            for version_entry in history.iter() {
                versions.push(version_entry.clone());
            }
        }
        
        // Sort by version (descending - newest first)
        versions.sort_by(|a, b| b.version.cmp(&a.version));
        versions
    }

    pub fn delete_config(&self, user_id: Uuid, config_key: &str) -> bool {
        let key = (user_id, config_key.to_string());
        let removed_config = self.configs.remove(&key);
        
        if removed_config.is_some() {
            // Also remove history
            self.history.remove(&key);
            true
        } else {
            false
        }
    }

    pub fn list_user_configs(&self, user_id: Uuid) -> Vec<ConfigSummary> {
        let mut configs = Vec::new();
        
        for entry in self.configs.iter() {
            let ((entry_user_id, _), config) = (entry.key(), entry.value());
            
            if *entry_user_id == user_id {
                configs.push(ConfigSummary {
                    key: config.key.clone(),
                    description: config.description.clone(),
                    version: config.version,
                    created_at: config.created_at,
                    updated_at: config.updated_at,
                });
            }
        }
        
        // Sort by key for consistent ordering
        configs.sort_by(|a, b| a.key.cmp(&b.key));
        configs
    }
}

#[derive(Debug, Serialize)]
pub struct ConfigSummary {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct SetConfigRequest {
    pub value: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetConfigQuery {
    pub version: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ListConfigsResponse {
    pub configs: Vec<ConfigSummary>,
}

#[derive(Debug, Serialize)]
pub struct ConfigHistoryResponse {
    pub versions: Vec<ConfigVersion>,
}

// Route handlers

pub async fn set_config(
    Path(key): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<SetConfigRequest>,
) -> Result<Json<ConfigEntry>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    // Validate config key
    if key.is_empty() || key.len() > 100 {
        return Err(AppError::InvalidInput("Config key must be 1-100 characters".to_string()));
    }
    
    // Config key should be alphanumeric, hyphens, underscores, dots only
    if !key.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.') {
        return Err(AppError::InvalidInput("Config key can only contain letters, numbers, hyphens, underscores, and dots".to_string()));
    }
    
    // Validate description length
    if let Some(ref desc) = req.description {
        if desc.len() > 500 {
            return Err(AppError::InvalidInput("Description must be less than 500 characters".to_string()));
        }
    }
    
    // Check value size (limit to ~100KB JSON)
    let value_str = serde_json::to_string(&req.value)
        .map_err(|e| AppError::InvalidInput(format!("Invalid JSON value: {}", e)))?;
    
    if value_str.len() > 100_000 {
        return Err(AppError::InvalidInput("Config value too large (max 100KB)".to_string()));
    }
    
    let config = state.configstore_handlers.store.set_config(
        user_id,
        &key,
        req.value,
        req.description,
    )?;
    
    Ok(Json(config))
}

pub async fn get_config(
    Path(key): Path<String>,
    Query(query): Query<GetConfigQuery>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<ConfigEntry>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    if let Some(config) = state.configstore_handlers.store.get_config(user_id, &key, query.version) {
        Ok(Json(config))
    } else {
        Err(AppError::NotFound("Config not found".to_string()))
    }
}

pub async fn get_config_history(
    Path(key): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<ConfigHistoryResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let versions = state.configstore_handlers.store.get_config_history(user_id, &key);
    
    if versions.is_empty() {
        return Err(AppError::NotFound("Config not found".to_string()));
    }
    
    Ok(Json(ConfigHistoryResponse { versions }))
}

pub async fn delete_config(
    Path(key): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let deleted = state.configstore_handlers.store.delete_config(user_id, &key);
    
    if deleted {
        Ok(Json(serde_json::json!({"deleted": true})))
    } else {
        Err(AppError::NotFound("Config not found".to_string()))
    }
}

pub async fn list_configs(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<ListConfigsResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let configs = state.configstore_handlers.store.list_user_configs(user_id);
    
    Ok(Json(ListConfigsResponse { configs }))
}