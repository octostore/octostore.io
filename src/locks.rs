use crate::{
    auth::AuthService,
    error::{AppError, Result},
    models::{
        validate_lock_name, validate_metadata, validate_ttl, AcquireLockRequest,
        AcquireLockResponse, LockStatusResponse, ReleaseLockRequest, RenewLockRequest,
        RenewLockResponse, UserLockInfo, UserLocksResponse,
    },
    store::LockStore,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use tracing::info;

#[derive(Clone)]
pub struct LockHandlers {
    pub store: LockStore,
    pub auth: AuthService,
}

impl LockHandlers {
    pub fn new(store: LockStore, auth: AuthService) -> Self {
        Self { store, auth }
    }
}

pub async fn acquire_lock(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<AcquireLockRequest>,
) -> Result<Json<AcquireLockResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    // Validate lock name
    validate_lock_name(&name)
        .map_err(|reason| AppError::InvalidLockName { reason })?;
    
    // Validate TTL
    let ttl_seconds = req.ttl_seconds.unwrap_or(60);
    validate_ttl(ttl_seconds)
        .map_err(|reason| AppError::InvalidTtl { reason })?;
    
    // Validate metadata
    validate_metadata(&req.metadata)
        .map_err(|reason| AppError::InvalidInput(reason))?;
    
    // Check user lock limit (max 100)
    let current_lock_count = state.lock_handlers.store.count_user_locks(user_id);
    if current_lock_count >= 100 {
        // Check if this specific lock is already held by the user (idempotent case)
        if let Some(existing_lock) = state.lock_handlers.store.get_lock(&name) {
            if existing_lock.holder_id == user_id && !existing_lock.is_expired() {
                return Ok(Json(AcquireLockResponse::Acquired {
                    lease_id: existing_lock.lease_id,
                    fencing_token: existing_lock.fencing_token,
                    expires_at: existing_lock.expires_at,
                    metadata: existing_lock.metadata.clone(),
                }));
            }
        }
        return Err(AppError::LockLimitExceeded);
    }
    
    match state.lock_handlers.store.acquire_lock(name.clone(), user_id, ttl_seconds, req.metadata.clone()) {
        Ok((lease_id, fencing_token, expires_at)) => {
            // Only increment counter for new lock acquisitions, not idempotent renewals
            // We can check if this is a new acquisition by seeing if the fencing token changed
            state.metrics.record_lock_operation("acquire");
            info!("Lock acquired: {} by user {}", name, user_id);
            Ok(Json(AcquireLockResponse::Acquired {
                lease_id,
                fencing_token,
                expires_at,
                metadata: req.metadata.clone(),
            }))
        }
        Err(AppError::LockHeld) => {
            // Return info about who holds it
            if let Some(lock) = state.lock_handlers.store.get_lock(&name) {
                Ok(Json(AcquireLockResponse::Held {
                    holder_id: lock.holder_id,
                    expires_at: lock.expires_at,
                    metadata: lock.metadata.clone(),
                }))
            } else {
                // This shouldn't happen, but handle gracefully
                Err(AppError::Internal(anyhow::anyhow!("Lock state inconsistent")))
            }
        }
        Err(e) => Err(e),
    }
}

pub async fn release_lock(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<ReleaseLockRequest>,
) -> Result<Json<()>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    validate_lock_name(&name)
        .map_err(|reason| AppError::InvalidLockName { reason })?;
    
    state.lock_handlers.store.release_lock(&name, req.lease_id, user_id)?;
    
    // Increment release counter
    state.metrics.record_lock_operation("release");
    info!("Lock released: {} by user {}", name, user_id);
    Ok(Json(()))
}

pub async fn renew_lock(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<RenewLockRequest>,
) -> Result<Json<RenewLockResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    validate_lock_name(&name)
        .map_err(|reason| AppError::InvalidLockName { reason })?;
    
    let ttl_seconds = req.ttl_seconds.unwrap_or(60);
    validate_ttl(ttl_seconds)
        .map_err(|reason| AppError::InvalidTtl { reason })?;
    
    let expires_at = state.lock_handlers.store.renew_lock(&name, req.lease_id, user_id, ttl_seconds)?;
    
    info!("Lock renewed: {} by user {}", name, user_id);
    Ok(Json(RenewLockResponse {
        lease_id: req.lease_id,
        expires_at,
    }))
}

pub async fn get_lock_status(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<LockStatusResponse>> {
    let _user_id = state.auth_service.authenticate(&headers)?; // Auth required but user_id not used
    
    validate_lock_name(&name)
        .map_err(|reason| AppError::InvalidLockName { reason })?;
    
    if let Some(lock) = state.lock_handlers.store.get_lock(&name) {
        if lock.is_expired() {
            // Lock exists but is expired, treat as free
            Ok(Json(LockStatusResponse {
                name: name.clone(),
                status: "free".to_string(),
                holder_id: None,
                fencing_token: lock.fencing_token, // Keep the last known fencing token
                expires_at: None,
                metadata: None, // Expired lock, no metadata
            }))
        } else {
            Ok(Json(LockStatusResponse {
                name: name.clone(),
                status: "held".to_string(),
                holder_id: Some(lock.holder_id),
                fencing_token: lock.fencing_token,
                expires_at: Some(lock.expires_at),
                metadata: lock.metadata.clone(),
            }))
        }
    } else {
        // Lock doesn't exist, it's free
        // We need to determine what fencing token would be used next
        let next_fencing_token = state.lock_handlers.store.get_fencing_counter() + 1;
        Ok(Json(LockStatusResponse {
            name: name.clone(),
            status: "free".to_string(),
            holder_id: None,
            fencing_token: next_fencing_token,
            expires_at: None,
            metadata: None,
        }))
    }
}

pub async fn list_user_locks(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<UserLocksResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    
    let locks = state.lock_handlers.store.get_user_locks(user_id);
    let lock_infos: Vec<UserLockInfo> = locks
        .into_iter()
        .map(|lock| UserLockInfo {
            name: lock.name,
            lease_id: lock.lease_id,
            fencing_token: lock.fencing_token,
            expires_at: lock.expires_at,
            metadata: lock.metadata,
        })
        .collect();
    
    Ok(Json(UserLocksResponse { locks: lock_infos }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    fn create_test_handlers() -> (LockHandlers, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();

        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: db_path.clone(),
            github_client_id: "test_client_id".to_string(),
            github_client_secret: "test_client_secret".to_string(),
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: Some("test_admin_key".to_string()),
        };

        let auth_service = AuthService::new(config).unwrap();
        let store = LockStore::new(&db_path, 1).unwrap();

        (LockHandlers::new(store, auth_service), temp_file)
    }

    #[test]
    fn test_new_handlers_have_no_locks() {
        let (handlers, _tmp) = create_test_handlers();
        assert_eq!(handlers.store.count_user_locks(Uuid::new_v4()), 0);
    }

    #[test]
    fn test_cloned_handlers_share_state() {
        let (handlers, _tmp) = create_test_handlers();
        let cloned = handlers.clone();
        let user_id = Uuid::new_v4();

        handlers.store.acquire_lock("shared-test".into(), user_id, 60, None).unwrap();
        assert_eq!(cloned.store.count_user_locks(user_id), 1,
            "cloned handlers should see locks created through the original");
    }
}