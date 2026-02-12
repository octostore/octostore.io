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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_feature_flag_store_new() {
        let store = FeatureFlagStore::new();
        assert_eq!(store.flags.len(), 0);
    }

    #[test]
    fn test_set_flag_new() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        let flag = store.set_flag(user_id, "test_flag", true, None).unwrap();
        
        assert_eq!(flag.name, "test_flag");
        assert!(flag.enabled);
        assert_eq!(flag.percentage, None);
        assert_eq!(store.flags.len(), 1);
    }

    #[test]
    fn test_set_flag_with_percentage() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        let flag = store.set_flag(user_id, "test_flag", true, Some(50)).unwrap();
        
        assert_eq!(flag.name, "test_flag");
        assert!(flag.enabled);
        assert_eq!(flag.percentage, Some(50));
    }

    #[test]
    fn test_set_flag_invalid_percentage() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        let result = store.set_flag(user_id, "test_flag", true, Some(101));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Percentage must be 0-100"));
    }

    #[test]
    fn test_set_flag_update_existing() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        // Create initial flag
        let flag1 = store.set_flag(user_id, "test_flag", true, Some(30)).unwrap();
        let original_created_at = flag1.created_at;
        
        // Update the same flag
        let flag2 = store.set_flag(user_id, "test_flag", false, Some(70)).unwrap();
        
        assert_eq!(flag2.name, "test_flag");
        assert!(!flag2.enabled);
        assert_eq!(flag2.percentage, Some(70));
        assert_eq!(flag2.created_at, original_created_at); // Should not change
        assert!(flag2.updated_at > flag2.created_at);      // Should be updated
        assert_eq!(store.flags.len(), 1); // Should still be just one flag
    }

    #[test]
    fn test_set_flag_limit_exceeded() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        // Create 1000 flags (the limit)
        for i in 0..1000 {
            let flag_name = format!("flag_{}", i);
            store.set_flag(user_id, &flag_name, true, None).unwrap();
        }
        
        // Try to create one more - should fail
        let result = store.set_flag(user_id, "flag_1000", true, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Maximum number of flags"));
        assert_eq!(store.flags.len(), 1000);
    }

    #[test]
    fn test_set_flag_limit_per_user() {
        let store = FeatureFlagStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        
        // User1 creates 1000 flags
        for i in 0..1000 {
            let flag_name = format!("flag_{}", i);
            store.set_flag(user1, &flag_name, true, None).unwrap();
        }
        
        // User2 should still be able to create flags
        let flag = store.set_flag(user2, "user2_flag", true, None).unwrap();
        assert_eq!(flag.name, "user2_flag");
        assert_eq!(store.flags.len(), 1001);
    }

    #[test]
    fn test_get_flag_simple_enabled() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        store.set_flag(user_id, "test_flag", true, None).unwrap();
        
        let evaluation = store.get_flag(user_id, "test_flag").unwrap();
        assert_eq!(evaluation.name, "test_flag");
        assert!(evaluation.enabled);
    }

    #[test]
    fn test_get_flag_simple_disabled() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        store.set_flag(user_id, "test_flag", false, None).unwrap();
        
        let evaluation = store.get_flag(user_id, "test_flag").unwrap();
        assert_eq!(evaluation.name, "test_flag");
        assert!(!evaluation.enabled);
    }

    #[test]
    fn test_get_flag_nonexistent() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        let evaluation = store.get_flag(user_id, "nonexistent");
        assert!(evaluation.is_none());
    }

    #[test]
    fn test_get_flag_with_percentage_deterministic() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        store.set_flag(user_id, "test_flag", true, Some(50)).unwrap();
        
        // Same user should get consistent results
        let eval1 = store.get_flag(user_id, "test_flag").unwrap();
        let eval2 = store.get_flag(user_id, "test_flag").unwrap();
        assert_eq!(eval1.enabled, eval2.enabled);
    }

    #[test]
    fn test_get_flag_percentage_distribution() {
        let store = FeatureFlagStore::new();
        
        // Test with 0% and 100% to ensure edge cases work
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        
        store.set_flag(user1, "zero_percent", true, Some(0)).unwrap();
        store.set_flag(user2, "hundred_percent", true, Some(100)).unwrap();
        
        let eval_zero = store.get_flag(user1, "zero_percent").unwrap();
        let eval_hundred = store.get_flag(user2, "hundred_percent").unwrap();
        
        assert!(!eval_zero.enabled);
        assert!(eval_hundred.enabled);
    }

    #[test]
    fn test_get_flag_disabled_with_percentage() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        // Even with 100% rollout, disabled flag should remain disabled
        store.set_flag(user_id, "disabled_flag", false, Some(100)).unwrap();
        
        let evaluation = store.get_flag(user_id, "disabled_flag").unwrap();
        assert!(!evaluation.enabled);
    }

    #[test]
    fn test_delete_flag_existing() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        store.set_flag(user_id, "test_flag", true, None).unwrap();
        assert_eq!(store.flags.len(), 1);
        
        let deleted = store.delete_flag(user_id, "test_flag");
        assert!(deleted);
        assert_eq!(store.flags.len(), 0);
        
        // Should no longer be retrievable
        let evaluation = store.get_flag(user_id, "test_flag");
        assert!(evaluation.is_none());
    }

    #[test]
    fn test_delete_flag_nonexistent() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        let deleted = store.delete_flag(user_id, "nonexistent");
        assert!(!deleted);
    }

    #[test]
    fn test_delete_flag_isolation() {
        let store = FeatureFlagStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        
        store.set_flag(user1, "shared_name", true, None).unwrap();
        store.set_flag(user2, "shared_name", false, None).unwrap();
        
        // Delete user1's flag
        let deleted = store.delete_flag(user1, "shared_name");
        assert!(deleted);
        
        // User2's flag should still exist
        let user2_eval = store.get_flag(user2, "shared_name").unwrap();
        assert!(!user2_eval.enabled);
        assert_eq!(store.flags.len(), 1);
    }

    #[test]
    fn test_list_user_flags_empty() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        let flags = store.list_user_flags(user_id);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_list_user_flags_single_user() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        store.set_flag(user_id, "flag1", true, None).unwrap();
        store.set_flag(user_id, "flag2", false, Some(75)).unwrap();
        store.set_flag(user_id, "flag3", true, Some(25)).unwrap();
        
        let flags = store.list_user_flags(user_id);
        assert_eq!(flags.len(), 3);
        
        // Should be sorted by name
        assert_eq!(flags[0].name, "flag1");
        assert_eq!(flags[1].name, "flag2");
        assert_eq!(flags[2].name, "flag3");
        
        // Check first flag details
        assert!(flags[0].enabled);
        assert_eq!(flags[0].percentage, None);
        
        // Check second flag details
        assert!(!flags[1].enabled);
        assert_eq!(flags[1].percentage, Some(75));
        
        // Check third flag details
        assert!(flags[2].enabled);
        assert_eq!(flags[2].percentage, Some(25));
    }

    #[test]
    fn test_list_user_flags_isolation() {
        let store = FeatureFlagStore::new();
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();
        
        store.set_flag(user1, "user1_flag", true, None).unwrap();
        store.set_flag(user2, "user2_flag", false, None).unwrap();
        store.set_flag(user1, "shared_name", true, Some(50)).unwrap();
        store.set_flag(user2, "shared_name", false, Some(80)).unwrap();
        
        let user1_flags = store.list_user_flags(user1);
        let user2_flags = store.list_user_flags(user2);
        
        assert_eq!(user1_flags.len(), 2);
        assert_eq!(user2_flags.len(), 2);
        
        // Check user1 flags
        let user1_names: HashSet<_> = user1_flags.iter().map(|f| &f.name).collect();
        assert!(user1_names.contains(&"user1_flag".to_string()));
        assert!(user1_names.contains(&"shared_name".to_string()));
        
        // Check user2 flags
        let user2_names: HashSet<_> = user2_flags.iter().map(|f| &f.name).collect();
        assert!(user2_names.contains(&"user2_flag".to_string()));
        assert!(user2_names.contains(&"shared_name".to_string()));
        
        // Check that shared_name has different settings for each user
        let user1_shared = user1_flags.iter().find(|f| f.name == "shared_name").unwrap();
        let user2_shared = user2_flags.iter().find(|f| f.name == "shared_name").unwrap();
        
        assert!(user1_shared.enabled);
        assert_eq!(user1_shared.percentage, Some(50));
        assert!(!user2_shared.enabled);
        assert_eq!(user2_shared.percentage, Some(80));
    }

    #[test]
    fn test_serialization() {
        // Test FlagEvaluation serialization
        let eval = FlagEvaluation {
            name: "test_flag".to_string(),
            enabled: true,
        };
        let json = serde_json::to_string(&eval).unwrap();
        assert!(json.contains("\"name\":\"test_flag\""));
        assert!(json.contains("\"enabled\":true"));
        
        // Test UserFeatureFlag serialization
        let user_flag = UserFeatureFlag {
            name: "test_flag".to_string(),
            enabled: false,
            percentage: Some(60),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&user_flag).unwrap();
        assert!(json.contains("\"enabled\":false"));
        assert!(json.contains("\"percentage\":60"));
        
        // Test UserFeatureFlag without percentage
        let user_flag_no_pct = UserFeatureFlag {
            name: "test_flag".to_string(),
            enabled: true,
            percentage: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&user_flag_no_pct).unwrap();
        assert!(!json.contains("\"percentage\":"));
    }

    #[test]
    fn test_request_deserialization() {
        // Test SetFlagRequest with percentage
        let json = r#"{"enabled":true,"percentage":75}"#;
        let req: SetFlagRequest = serde_json::from_str(json).unwrap();
        assert!(req.enabled);
        assert_eq!(req.percentage, Some(75));
        
        // Test SetFlagRequest without percentage
        let json = r#"{"enabled":false}"#;
        let req: SetFlagRequest = serde_json::from_str(json).unwrap();
        assert!(!req.enabled);
        assert_eq!(req.percentage, None);
        
        // Test SetFlagRequest with null percentage
        let json = r#"{"enabled":true,"percentage":null}"#;
        let req: SetFlagRequest = serde_json::from_str(json).unwrap();
        assert!(req.enabled);
        assert_eq!(req.percentage, None);
    }

    #[test]
    fn test_hash_consistency_across_calls() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        // Create flag with 50% rollout
        store.set_flag(user_id, "consistent_flag", true, Some(50)).unwrap();
        
        // Get the flag multiple times - should be consistent
        let results: Vec<bool> = (0..10)
            .map(|_| store.get_flag(user_id, "consistent_flag").unwrap().enabled)
            .collect();
        
        // All results should be the same
        assert!(results.iter().all(|&x| x == results[0]));
    }

    #[test]
    fn test_hash_distribution_different_users() {
        let store = FeatureFlagStore::new();
        let flag_name = "distribution_test";
        
        // Create many users with the same flag (50% rollout)
        let mut enabled_count = 0;
        let total_users = 1000;
        
        for _ in 0..total_users {
            let user_id = Uuid::new_v4();
            store.set_flag(user_id, flag_name, true, Some(50)).unwrap();
            
            if store.get_flag(user_id, flag_name).unwrap().enabled {
                enabled_count += 1;
            }
        }
        
        // Should be roughly 50% (allow for some variance due to randomness of UUIDs)
        let percentage = (enabled_count as f64 / total_users as f64) * 100.0;
        assert!(percentage >= 40.0 && percentage <= 60.0, 
                "Expected ~50%, got {:.1}%", percentage);
    }

    #[test]
    fn test_edge_cases() {
        let store = FeatureFlagStore::new();
        let user_id = Uuid::new_v4();
        
        // Test 0% rollout
        store.set_flag(user_id, "zero_percent", true, Some(0)).unwrap();
        let eval = store.get_flag(user_id, "zero_percent").unwrap();
        assert!(!eval.enabled);
        
        // Test 100% rollout
        store.set_flag(user_id, "hundred_percent", true, Some(100)).unwrap();
        let eval = store.get_flag(user_id, "hundred_percent").unwrap();
        assert!(eval.enabled);
        
        // Test that percentage is ignored when flag is disabled
        store.set_flag(user_id, "disabled_hundred", false, Some(100)).unwrap();
        let eval = store.get_flag(user_id, "disabled_hundred").unwrap();
        assert!(!eval.enabled);
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;
        
        let store = Arc::new(FeatureFlagStore::new());
        let user_id = Uuid::new_v4();
        let mut handles = vec![];
        
        // Spawn multiple threads setting flags
        for i in 0..10 {
            let store_clone = store.clone();
            let user_id_clone = user_id;
            
            let handle = thread::spawn(move || {
                for j in 0..10 {
                    let flag_name = format!("flag_{}_{}", i, j);
                    store_clone.set_flag(user_id_clone, &flag_name, true, Some(50)).unwrap();
                }
            });
            handles.push(handle);
        }
        
        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Should have 100 flags total
        let flags = store.list_user_flags(user_id);
        assert_eq!(flags.len(), 100);
        assert_eq!(store.flags.len(), 100);
    }
}