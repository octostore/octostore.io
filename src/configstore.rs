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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn test_config_store_new() {
        let store = ConfigStore::new();
        assert_eq!(store.configs.len(), 0);
        assert_eq!(store.history.len(), 0);
    }

    #[test]
    fn test_set_config_new() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let value = json!({"key": "value", "number": 42});
        
        let config = store.set_config(user_id, "test_config", value.clone(), Some("Test description".to_string())).unwrap();
        
        assert_eq!(config.key, "test_config");
        assert_eq!(config.value, value);
        assert_eq!(config.description, Some("Test description".to_string()));
        assert_eq!(config.version, 1);
        assert_eq!(store.configs.len(), 1);
    }

    #[test]
    fn test_set_config_update_existing() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        // Create initial config
        let value1 = json!({"initial": "value"});
        let config1 = store.set_config(user_id, "test_config", value1.clone(), Some("Initial".to_string())).unwrap();
        let original_created_at = config1.created_at;
        
        // Update the config
        let value2 = json!({"updated": "value"});
        let config2 = store.set_config(user_id, "test_config", value2.clone(), Some("Updated".to_string())).unwrap();
        
        assert_eq!(config2.key, "test_config");
        assert_eq!(config2.value, value2);
        assert_eq!(config2.description, Some("Updated".to_string()));
        assert_eq!(config2.version, 2);
        assert_eq!(config2.created_at, original_created_at); // Should not change
        assert!(config2.updated_at > config2.created_at);   // Should be updated
        assert_eq!(store.configs.len(), 1); // Still just one config entry
        
        // History should have the previous version
        let key = (user_id, "test_config".to_string());
        assert!(store.history.contains_key(&key));
        let history = store.history.get(&key).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].version, 1);
        assert_eq!(history[0].value, value1);
    }

    #[test]
    fn test_set_config_limit_exceeded() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        // Create 500 configs (the limit)
        for i in 0..500 {
            let key = format!("config_{}", i);
            let value = json!({"index": i});
            store.set_config(user_id, &key, value, None).unwrap();
        }
        
        // Try to create one more - should fail
        let result = store.set_config(user_id, "config_500", json!({"index": 500}), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Maximum number of config keys"));
        assert_eq!(store.configs.len(), 500);
    }

    #[test]
    fn test_set_config_limit_per_user() {
        let store = ConfigStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        
        // User1 creates 500 configs
        for i in 0..500 {
            let key = format!("config_{}", i);
            store.set_config(user1, &key, json!({"value": i}), None).unwrap();
        }
        
        // User2 should still be able to create configs
        let config = store.set_config(user2, "user2_config", json!({"value": "test"}), None).unwrap();
        assert_eq!(config.key, "user2_config");
        assert_eq!(store.configs.len(), 501);
    }

    #[test]
    fn test_set_config_history_limit() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let config_key = "test_config";
        
        // Create initial config and then update it 15 times
        store.set_config(user_id, config_key, json!({"version": 0}), None).unwrap();
        
        for i in 1..=15 {
            let value = json!({"version": i});
            store.set_config(user_id, config_key, value, None).unwrap();
        }
        
        let key = (user_id, config_key.to_string());
        let history = store.history.get(&key).unwrap();
        
        // Should only keep last 10 versions in history (versions 5-14, current is 15)
        assert_eq!(history.len(), 10);
        
        let versions: Vec<u32> = history.iter().map(|v| v.version).collect();
        assert_eq!(versions, vec![6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn test_get_config_current() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let value = json!({"test": "value"});
        
        store.set_config(user_id, "test_config", value.clone(), None).unwrap();
        
        let retrieved = store.get_config(user_id, "test_config", None).unwrap();
        assert_eq!(retrieved.value, value);
        assert_eq!(retrieved.version, 1);
    }

    #[test]
    fn test_get_config_nonexistent() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        let result = store.get_config(user_id, "nonexistent", None);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_config_specific_version() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let config_key = "test_config";
        
        // Create and update config multiple times
        let value1 = json!({"version": 1});
        store.set_config(user_id, config_key, value1.clone(), None).unwrap();
        
        let value2 = json!({"version": 2});
        store.set_config(user_id, config_key, value2.clone(), None).unwrap();
        
        let value3 = json!({"version": 3});
        store.set_config(user_id, config_key, value3.clone(), None).unwrap();
        
        // Get specific versions
        let config_v1 = store.get_config(user_id, config_key, Some(1)).unwrap();
        assert_eq!(config_v1.value, value1);
        assert_eq!(config_v1.version, 1);
        
        let config_v2 = store.get_config(user_id, config_key, Some(2)).unwrap();
        assert_eq!(config_v2.value, value2);
        assert_eq!(config_v2.version, 2);
        
        let config_v3 = store.get_config(user_id, config_key, Some(3)).unwrap();
        assert_eq!(config_v3.value, value3);
        assert_eq!(config_v3.version, 3);
        
        // Non-existent version
        let config_v99 = store.get_config(user_id, config_key, Some(99));
        assert!(config_v99.is_none());
    }

    #[test]
    fn test_get_config_user_isolation() {
        let store = ConfigStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        let config_key = "shared_key";
        
        let value1 = json!({"user": 1});
        let value2 = json!({"user": 2});
        
        store.set_config(user1, config_key, value1.clone(), None).unwrap();
        store.set_config(user2, config_key, value2.clone(), None).unwrap();
        
        let config1 = store.get_config(user1, config_key, None).unwrap();
        let config2 = store.get_config(user2, config_key, None).unwrap();
        
        assert_eq!(config1.value, value1);
        assert_eq!(config2.value, value2);
    }

    #[test]
    fn test_get_config_history() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let config_key = "test_config";
        
        // Create and update config multiple times
        let value1 = json!({"version": 1});
        store.set_config(user_id, config_key, value1.clone(), Some("Version 1".to_string())).unwrap();
        
        let value2 = json!({"version": 2});
        store.set_config(user_id, config_key, value2.clone(), Some("Version 2".to_string())).unwrap();
        
        let value3 = json!({"version": 3});
        store.set_config(user_id, config_key, value3.clone(), Some("Version 3".to_string())).unwrap();
        
        let history = store.get_config_history(user_id, config_key);
        assert_eq!(history.len(), 3);
        
        // Should be sorted by version descending (newest first)
        assert_eq!(history[0].version, 3);
        assert_eq!(history[0].value, value3);
        assert_eq!(history[0].description, Some("Version 3".to_string()));
        
        assert_eq!(history[1].version, 2);
        assert_eq!(history[1].value, value2);
        assert_eq!(history[1].description, Some("Version 2".to_string()));
        
        assert_eq!(history[2].version, 1);
        assert_eq!(history[2].value, value1);
        assert_eq!(history[2].description, Some("Version 1".to_string()));
    }

    #[test]
    fn test_get_config_history_empty() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        let history = store.get_config_history(user_id, "nonexistent");
        assert!(history.is_empty());
    }

    #[test]
    fn test_delete_config_existing() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let config_key = "test_config";
        
        // Create config and history
        store.set_config(user_id, config_key, json!({"v": 1}), None).unwrap();
        store.set_config(user_id, config_key, json!({"v": 2}), None).unwrap();
        
        assert_eq!(store.configs.len(), 1);
        assert_eq!(store.history.len(), 1);
        
        // Delete the config
        let deleted = store.delete_config(user_id, config_key);
        assert!(deleted);
        assert_eq!(store.configs.len(), 0);
        assert_eq!(store.history.len(), 0);
        
        // Should no longer be retrievable
        let config = store.get_config(user_id, config_key, None);
        assert!(config.is_none());
    }

    #[test]
    fn test_delete_config_nonexistent() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        let deleted = store.delete_config(user_id, "nonexistent");
        assert!(!deleted);
    }

    #[test]
    fn test_delete_config_user_isolation() {
        let store = ConfigStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        let config_key = "shared_key";
        
        store.set_config(user1, config_key, json!({"user": 1}), None).unwrap();
        store.set_config(user2, config_key, json!({"user": 2}), None).unwrap();
        
        // Delete user1's config
        let deleted = store.delete_config(user1, config_key);
        assert!(deleted);
        
        // User2's config should still exist
        let config2 = store.get_config(user2, config_key, None).unwrap();
        assert_eq!(config2.value, json!({"user": 2}));
        assert_eq!(store.configs.len(), 1);
    }

    #[test]
    fn test_list_user_configs_empty() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        let configs = store.list_user_configs(user_id);
        assert!(configs.is_empty());
    }

    #[test]
    fn test_list_user_configs_single_user() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        store.set_config(user_id, "config1", json!({"test": 1}), Some("Description 1".to_string())).unwrap();
        store.set_config(user_id, "config2", json!({"test": 2}), None).unwrap();
        store.set_config(user_id, "config3", json!({"test": 3}), Some("Description 3".to_string())).unwrap();
        
        let configs = store.list_user_configs(user_id);
        assert_eq!(configs.len(), 3);
        
        // Should be sorted by key
        assert_eq!(configs[0].key, "config1");
        assert_eq!(configs[1].key, "config2");
        assert_eq!(configs[2].key, "config3");
        
        // Check descriptions
        assert_eq!(configs[0].description, Some("Description 1".to_string()));
        assert_eq!(configs[1].description, None);
        assert_eq!(configs[2].description, Some("Description 3".to_string()));
        
        // All should be version 1
        assert!(configs.iter().all(|c| c.version == 1));
    }

    #[test]
    fn test_list_user_configs_isolation() {
        let store = ConfigStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        
        store.set_config(user1, "user1_config", json!({"value": 1}), None).unwrap();
        store.set_config(user2, "user2_config", json!({"value": 2}), None).unwrap();
        store.set_config(user1, "shared_name", json!({"user": 1}), None).unwrap();
        store.set_config(user2, "shared_name", json!({"user": 2}), None).unwrap();
        
        let user1_configs = store.list_user_configs(user1);
        let user2_configs = store.list_user_configs(user2);
        
        assert_eq!(user1_configs.len(), 2);
        assert_eq!(user2_configs.len(), 2);
        
        // Check user1 configs
        let user1_names: HashSet<_> = user1_configs.iter().map(|c| &c.key).collect();
        assert!(user1_names.contains(&"user1_config".to_string()));
        assert!(user1_names.contains(&"shared_name".to_string()));
        
        // Check user2 configs
        let user2_names: HashSet<_> = user2_configs.iter().map(|c| &c.key).collect();
        assert!(user2_names.contains(&"user2_config".to_string()));
        assert!(user2_names.contains(&"shared_name".to_string()));
    }

    #[test]
    fn test_serialization() {
        let config_entry = ConfigEntry {
            key: "test_key".to_string(),
            value: json!({"nested": {"key": "value"}}),
            description: Some("Test description".to_string()),
            version: 5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        let json = serde_json::to_string(&config_entry).unwrap();
        assert!(json.contains("\"key\":\"test_key\""));
        assert!(json.contains("\"version\":5"));
        assert!(json.contains("\"description\":\"Test description\""));
        
        // Test ConfigSummary without description
        let summary = ConfigSummary {
            key: "test_key".to_string(),
            description: None,
            version: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("\"description\":")); // Should be omitted when None
    }

    #[test]
    fn test_request_deserialization() {
        // Test SetConfigRequest
        let json = r#"{"value":{"key":"value","number":42},"description":"Test desc"}"#;
        let req: SetConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.value, json!({"key":"value","number":42}));
        assert_eq!(req.description, Some("Test desc".to_string()));
        
        // Test without description
        let json = r#"{"value":true}"#;
        let req: SetConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.value, json!(true));
        assert_eq!(req.description, None);
        
        // Test GetConfigQuery
        let query = serde_json::from_str::<GetConfigQuery>(r#"{"version":5}"#).unwrap();
        assert_eq!(query.version, Some(5));
        
        let query = serde_json::from_str::<GetConfigQuery>(r#"{}"#).unwrap();
        assert_eq!(query.version, None);
    }

    #[test]
    fn test_complex_value_types() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        // Test various JSON value types
        let test_cases = vec![
            ("string_config", json!("simple string")),
            ("number_config", json!(42)),
            ("float_config", json!(3.14159)),
            ("bool_config", json!(true)),
            ("null_config", json!(null)),
            ("array_config", json!([1, 2, 3, "four"])),
            ("object_config", json!({"nested": {"deeply": {"key": "value"}}})),
            ("mixed_config", json!({"array": [1, 2, {"nested": true}], "string": "test"})),
        ];
        
        for (key, value) in &test_cases {
            store.set_config(user_id, key, value.clone(), None).unwrap();
            let retrieved = store.get_config(user_id, key, None).unwrap();
            assert_eq!(retrieved.value, *value);
        }
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;
        
        let store = Arc::new(ConfigStore::new());
        let user_id = Uuid::new_v4();
        let mut handles = vec![];
        
        // Spawn multiple threads setting configs
        for i in 0..10 {
            let store_clone = store.clone();
            let user_id_clone = user_id;
            
            let handle = thread::spawn(move || {
                for j in 0..10 {
                    let key = format!("config_{}_{}", i, j);
                    let value = json!({"thread": i, "iteration": j});
                    store_clone.set_config(user_id_clone, &key, value, None).unwrap();
                }
            });
            handles.push(handle);
        }
        
        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Should have 100 configs total
        let configs = store.list_user_configs(user_id);
        assert_eq!(configs.len(), 100);
        assert_eq!(store.configs.len(), 100);
    }

    #[test]
    fn test_edge_cases() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        
        // Test empty object
        store.set_config(user_id, "empty_object", json!({}), None).unwrap();
        let retrieved = store.get_config(user_id, "empty_object", None).unwrap();
        assert_eq!(retrieved.value, json!({}));
        
        // Test empty array
        store.set_config(user_id, "empty_array", json!([]), None).unwrap();
        let retrieved = store.get_config(user_id, "empty_array", None).unwrap();
        assert_eq!(retrieved.value, json!([]));
        
        // Test empty string
        store.set_config(user_id, "empty_string", json!(""), None).unwrap();
        let retrieved = store.get_config(user_id, "empty_string", None).unwrap();
        assert_eq!(retrieved.value, json!(""));
        
        // Test updating with same value should still increment version
        store.set_config(user_id, "same_value", json!("test"), None).unwrap();
        store.set_config(user_id, "same_value", json!("test"), None).unwrap();
        let retrieved = store.get_config(user_id, "same_value", None).unwrap();
        assert_eq!(retrieved.version, 2);
    }

    #[test]
    fn test_version_ordering_edge_cases() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let config_key = "version_test";
        
        // Create many versions to test ordering
        for i in 1..=5 {
            let value = json!({"version": i});
            store.set_config(user_id, config_key, value, None).unwrap();
        }
        
        let history = store.get_config_history(user_id, config_key);
        assert_eq!(history.len(), 5);
        
        // Verify descending order
        for i in 0..history.len() {
            assert_eq!(history[i].version, (5 - i) as u32);
        }
    }

    #[test]
    fn test_memory_cleanup_on_delete() {
        let store = ConfigStore::new();
        let user_id = Uuid::new_v4();
        let config_key = "cleanup_test";
        
        // Create config with history
        for i in 1..=5 {
            let value = json!({"version": i});
            store.set_config(user_id, config_key, value, None).unwrap();
        }
        
        // Verify both configs and history exist
        assert_eq!(store.configs.len(), 1);
        assert_eq!(store.history.len(), 1);
        
        // Delete should clean up both
        store.delete_config(user_id, config_key);
        assert_eq!(store.configs.len(), 0);
        assert_eq!(store.history.len(), 0);
    }
}