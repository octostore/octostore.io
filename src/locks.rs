use crate::{
    error::{AppError, Result},
    models::{
        validate_lock_name, validate_metadata, validate_ttl, AcquireLockRequest,
        AcquireLockResponse, ListLocksResponse, LockAcl, LockStatusResponse,
        ReleaseLockRequest, RenewLockRequest, RenewLockResponse, UpdateLockAclRequest,
        UpdateLockAclResponse, UserLockInfo, UserLocksResponse,
    },
    store::LockStore,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use std::convert::Infallible;
use tracing::info;

#[derive(Clone)]
pub struct LockHandlers {
    pub store: LockStore,
}

impl LockHandlers {
    pub fn new(store: LockStore) -> Self {
        Self { store }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

fn normalize_acl(acl: &LockAcl) -> LockAcl {
    let mut acquire: Vec<String> = acl
        .acquire
        .iter()
        .map(|p| p.trim().to_lowercase())
        .collect();
    acquire.sort();
    acquire.dedup();
    LockAcl { acquire }
}

fn validate_acl(acl: &LockAcl) -> Result<()> {
    if acl.acquire.is_empty() {
        return Err(AppError::InvalidInput("acl.acquire must not be empty".to_string()));
    }

    for principal in &acl.acquire {
        let p = principal.trim();
        if p.is_empty() || !(p.starts_with("user:") || p.starts_with("token:")) {
            return Err(AppError::InvalidInput(
                "acl principals must use user:<github_username> or token:<token>".to_string(),
            ));
        }
    }
    Ok(())
}

fn caller_in_acl(acl: &LockAcl, username: Option<&str>, token: Option<&str>) -> bool {
    acl.acquire.iter().any(|principal| {
        if let Some(rest) = principal.strip_prefix("user:") {
            return username.map(|u| u.eq_ignore_ascii_case(rest)).unwrap_or(false);
        }
        if let Some(rest) = principal.strip_prefix("token:") {
            return token.map(|t| t == rest).unwrap_or(false);
        }
        false
    })
}

pub async fn acquire_lock(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<AcquireLockRequest>,
) -> Result<(StatusCode, Json<AcquireLockResponse>)> {
    let user_id = state.auth_service.authenticate(&headers)?;
    let is_admin = user_id == uuid::Uuid::nil();
    let caller_token = bearer_token(&headers);
    let caller_username = state.auth_service.get_user_by_id(&user_id.to_string())?;

    // Validate lock name
    validate_lock_name(&name)?;

    // Validate TTL
    let ttl_seconds = req.ttl_seconds.unwrap_or(60);
    validate_ttl(ttl_seconds)?;

    // Validate metadata
    validate_metadata(&req.metadata)?;

    let ephemeral = req.ephemeral.unwrap_or(false);
    let lock_delay_seconds = req.lock_delay_seconds.map(|d| d.clamp(0, 30)).unwrap_or(0);

    // Ephemeral locks require a session_id
    if ephemeral && req.session_id.is_none() {
        return Err(AppError::InvalidInput(
            "ephemeral locks require a session_id".to_string(),
        ));
    }

    // Validate session if provided
    if let Some(session_id) = req.session_id {
        let session = state
            .session_store
            .get_session(session_id)
            .ok_or(crate::error::AppError::SessionNotFound)?;
        if session.user_id != user_id {
            return Err(crate::error::AppError::SessionNotFound);
        }
        if session.is_expired() {
            return Err(crate::error::AppError::SessionExpired);
        }
    }

    let requested_acl = if let Some(acl) = req.acl.clone() {
        validate_acl(&acl)?;
        Some(normalize_acl(&acl))
    } else {
        None
    };
    let existing_acl = state.lock_handlers.store.get_lock_acl(&name)?;

    if let (Some(existing), Some(requested)) = (&existing_acl, &requested_acl) {
        if existing != requested {
            return Err(AppError::Conflict(
                "ACL already exists; update with PUT /locks/{name}/acl".to_string(),
            ));
        }
    }

    let effective_acl = existing_acl.clone().or(requested_acl.clone());
    if !is_admin {
        if let Some(acl) = &effective_acl {
            let allowed = caller_in_acl(acl, caller_username.as_deref(), caller_token.as_deref());
            if !allowed {
                return Err(AppError::Forbidden(
                    "caller is not allowed to acquire this lock".to_string(),
                ));
            }
        }
    }

    // Check if lock is in cooling period (lock delay / grace period)
    if let Some((available_at, delay)) = state.lock_handlers.store.check_cooling(&name) {
        return Ok((
            StatusCode::CONFLICT,
            Json(AcquireLockResponse::Delayed {
                available_at,
                lock_delay_seconds: delay,
            }),
        ));
    }

    // Check user lock limit (max 100)
    let current_lock_count = state.lock_handlers.store.count_user_locks(user_id);
    if current_lock_count >= 100 {
        // Check if this specific lock is already held by the user (idempotent case)
        if let Some(existing_lock) = state.lock_handlers.store.get_lock(&name) {
            if existing_lock.holder_id == user_id && !existing_lock.is_expired() {
                return Ok((StatusCode::OK, Json(AcquireLockResponse::Acquired {
                    lease_id: existing_lock.lease_id,
                    fencing_token: existing_lock.fencing_token,
                    expires_at: existing_lock.expires_at,
                    metadata: existing_lock.metadata.clone(),
                })));
            }
        }
        return Err(AppError::LockLimitExceeded);
    }

    match state.lock_handlers.store.acquire_lock(name.clone(), user_id, ttl_seconds, req.metadata.clone(), req.session_id, ephemeral, lock_delay_seconds) {
        Ok((lease_id, fencing_token, expires_at)) => {
            if existing_acl.is_none() {
                if let Some(acl) = requested_acl {
                    state.lock_handlers.store.set_lock_acl(&name, &acl)?;
                }
            }

            state.metrics.record_lock_operation("acquire");
            info!("Lock acquired: {} by user {}", name, user_id);
            Ok((StatusCode::OK, Json(AcquireLockResponse::Acquired {
                lease_id,
                fencing_token,
                expires_at,
                metadata: req.metadata.clone(),
            })))
        }
        Err(AppError::LockHeld) => {
            // Return info about who holds it
            if let Some(lock) = state.lock_handlers.store.get_lock(&name) {
                Ok((StatusCode::OK, Json(AcquireLockResponse::Held {
                    holder_id: lock.holder_id,
                    expires_at: lock.expires_at,
                    metadata: lock.metadata.clone(),
                })))
            } else {
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
    
    validate_lock_name(&name)?;

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
    
    validate_lock_name(&name)?;

    let ttl_seconds = req.ttl_seconds.unwrap_or(60);
    validate_ttl(ttl_seconds)?;
    
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
    
    validate_lock_name(&name)?;

    let acl = state.lock_handlers.store.get_lock_acl(&name)?;

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
                acl,
            }))
        } else {
            Ok(Json(LockStatusResponse {
                name: name.clone(),
                status: "held".to_string(),
                holder_id: Some(lock.holder_id),
                fencing_token: lock.fencing_token,
                expires_at: Some(lock.expires_at),
                metadata: lock.metadata.clone(),
                acl,
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
            acl,
        }))
    }
}

pub async fn update_lock_acl(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateLockAclRequest>,
) -> Result<Json<UpdateLockAclResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;
    let is_admin = user_id == uuid::Uuid::nil();

    validate_lock_name(&name)?;
    validate_acl(&req.acl)?;
    let acl = normalize_acl(&req.acl);

    if !is_admin {
        let lock = state
            .lock_handlers
            .store
            .get_lock(&name)
            .ok_or(AppError::LockNotFound { name: name.clone() })?;

        if lock.holder_id != user_id || lock.is_expired() {
            return Err(AppError::Forbidden(
                "only current lock holder or admin can update ACL".to_string(),
            ));
        }
    }

    state.lock_handlers.store.set_lock_acl(&name, &acl)?;
    Ok(Json(UpdateLockAclResponse { name, acl }))
}

/// Watches a lock for real-time state changes via Server-Sent Events (SSE).
pub async fn watch_lock(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    // Authenticate the user
    let _user_id = state.auth_service.authenticate(&headers)?;
    
    validate_lock_name(&name)?;

    let rx = state.lock_handlers.store.watch_lock(&name);
    
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|msg| async move {
            match msg {
                Ok(event) => {
                    let event_json = serde_json::to_string(&event).ok()?;
                    Some(Ok(Event::default().data(event_json)))
                }
                Err(_) => None, // Handle lag by dropping events
            }
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Debug, Deserialize)]
pub struct ListLocksQuery {
    pub prefix: Option<String>,
}

pub async fn list_locks(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListLocksQuery>,
) -> Result<Json<ListLocksResponse>> {
    let _user_id = state.auth_service.authenticate(&headers)?;

    let locks = state.lock_handlers.store.list_locks(query.prefix.as_deref());
    let lock_responses: Vec<LockStatusResponse> = locks
        .into_iter()
        .filter(|lock| !lock.is_expired())
        .map(|lock| LockStatusResponse {
            name: lock.name,
            status: "held".to_string(),
            holder_id: Some(lock.holder_id),
            fencing_token: lock.fencing_token,
            expires_at: Some(lock.expires_at),
            metadata: lock.metadata,
            acl: None,
        })
        .collect();
    let total = lock_responses.len();

    Ok(Json(ListLocksResponse {
        locks: lock_responses,
        total,
        prefix: query.prefix,
    }))
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
    use crate::auth::AuthService;
    use crate::config::Config;
    use tempfile::NamedTempFile;
    use uuid::Uuid;
    use rusqlite::Connection;

    fn create_test_handlers() -> (LockHandlers, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        let db: crate::store::DbConn =
            std::sync::Arc::new(std::sync::Mutex::new(Connection::open(&db_path).unwrap()));
        let store = LockStore::new(db, 1).unwrap();
        (LockHandlers::new(store), temp_file)
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

        handlers.store.acquire_lock("shared-test".into(), user_id, 60, None, None, false, 0).unwrap();
        assert_eq!(cloned.store.count_user_locks(user_id), 1,
            "cloned handlers should see locks created through the original");
    }

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use serde_json::{json, Value};

    async fn test_app() -> (axum::Router, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        
        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: db_path,
            github_client_id: None,
            github_client_secret: None,
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: Some("test_admin_key".to_string()),
            static_tokens: Some("testuser:testtoken,user2:token2".to_string()),
            static_tokens_file: None,
            admin_username: None,
        };

        // Share one DbConn between both services (#19)
        let db: crate::store::DbConn = std::sync::Arc::new(std::sync::Mutex::new(
            Connection::open(&config.database_url).unwrap(),
        ));
        let auth_service = AuthService::new(config.clone(), db.clone()).unwrap();
        auth_service.seed_static_tokens();
        let lock_store = LockStore::new(db.clone(), 0).unwrap();
        let lock_handlers = LockHandlers::new(lock_store.clone());
        let session_store = crate::sessions::SessionStore::new(db.clone()).unwrap();
        let webhook_store = crate::webhooks::WebhookStore::new(db).unwrap();

        let app_state = crate::app::AppState {
            lock_handlers,
            auth_service,
            config: config.clone(),
            metrics: crate::metrics::Metrics::new(),
            session_store,
            webhook_store,
        };

        let router = axum::Router::new()
            .route("/locks/:name/acquire", axum::routing::post(acquire_lock))
            .route("/locks/:name/acl", axum::routing::put(update_lock_acl))
            .route("/locks/:name/release", axum::routing::post(release_lock))
            .route("/locks/:name/renew", axum::routing::post(renew_lock))
            .route("/locks/:name/watch", axum::routing::get(watch_lock))
            .route("/locks/:name", axum::routing::get(get_lock_status))
            .with_state(app_state);

        (router, temp_file)
    }

    #[tokio::test]
    async fn test_acquire_status_release_roundtrip() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/test-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap()
        ).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        let lease_id = res["lease_id"].as_str().unwrap().to_string();

        // 2. Get Status
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/test-lock")
                .header("authorization", "Bearer testtoken")
                .body(Body::empty())
                .unwrap()
        ).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(res["status"], "held");

        // 3. Release
        let response = app.oneshot(
            Request::builder()
                .uri("/locks/test-lock/release")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"lease_id": lease_id}).to_string()))
                .unwrap()
        ).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_idempotent_reacquire() {
        let (app, _tmp) = test_app().await;

        let req = Request::builder()
            .uri("/locks/test-lock/acquire")
            .method("POST")
            .header("authorization", "Bearer testtoken")
            .header("content-type", "application/json")
            .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
            .unwrap();

        let response = app.clone().oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let res1: Value = serde_json::from_slice(&body).unwrap();

        let req2 = Request::builder()
            .uri("/locks/test-lock/acquire")
            .method("POST")
            .header("authorization", "Bearer testtoken")
            .header("content-type", "application/json")
            .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
            .unwrap();

        let response2 = app.oneshot(req2).await.unwrap();
        let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX).await.unwrap();
        let res2: Value = serde_json::from_slice(&body2).unwrap();

        assert_eq!(res1["lease_id"], res2["lease_id"], "Idempotent re-acquire should return same lease_id");
    }

    #[tokio::test]
    async fn test_release_wrong_lease() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/test-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap()
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. Release with wrong lease
        let response = app.oneshot(
            Request::builder()
                .uri("/locks/test-lock/release")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"lease_id": Uuid::new_v4()}).to_string()))
                .unwrap()
        ).await.unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_acquire_expired_lock() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire with 1s TTL
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/test-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 1}).to_string()))
                .unwrap()
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. Wait 2s
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // 3. Re-acquire should succeed (different lease_id)
        let response = app.oneshot(
            Request::builder()
                .uri("/locks/test-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap()
        ).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_acquire_renew_release_lifecycle() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/renew-test/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let acquired: Value = serde_json::from_slice(&body).unwrap();
        let lease_id = acquired["lease_id"].as_str().unwrap().to_string();

        // 2. Renew
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/renew-test/renew")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"lease_id": lease_id, "ttl_seconds": 120}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let renewed: Value = serde_json::from_slice(&body).unwrap();
        assert!(renewed["expires_at"].is_string(), "renew should return new expires_at");

        // 3. Release
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/renew-test/release")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"lease_id": lease_id}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_acquire_held_by_another_user() {
        let (app, _tmp) = test_app().await;

        // user1 acquires the lock
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/contested-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        // user1 should have acquired the lock
        assert!(res["lease_id"].is_string(), "user1 should acquire the lock");

        // user2 tries to acquire the same lock → 200 with "held" body (not lease_id)
        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/contested-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer token2")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        // The held response has holder_id but no lease_id
        assert!(res["holder_id"].is_string(), "held response should have holder_id");
        assert!(res["lease_id"].is_null(), "held response should not have lease_id");
    }

    #[tokio::test]
    async fn test_acl_blocks_non_members() {
        let (app, _tmp) = test_app().await;

        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/acl-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60, "acl": {"acquire": ["user:testuser"]}}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/acl-lock/acquire")
                .method("POST")
                .header("authorization", "Bearer token2")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_acl_update_requires_holder_or_admin() {
        let (app, _tmp) = test_app().await;

        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/acl-update/acquire")
                .method("POST")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/acl-update/acl")
                .method("PUT")
                .header("authorization", "Bearer token2")
                .header("content-type", "application/json")
                .body(Body::from(json!({"acl": {"acquire": ["user:user2"]}}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app.clone().oneshot(
            Request::builder()
                .uri("/locks/acl-update/acl")
                .method("PUT")
                .header("authorization", "Bearer testtoken")
                .header("content-type", "application/json")
                .body(Body::from(json!({"acl": {"acquire": ["user:testuser"]}}).to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
