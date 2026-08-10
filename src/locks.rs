use crate::{
    error::{AppError, Result},
    models::{
        validate_lock_name, validate_metadata, validate_ttl, AcquireLockRequest,
        AcquireLockResponse, ListLocksResponse, LockAcl, LockEventType, LockStatusResponse,
        LockWatchEvent, ReleaseLockRequest, RenewLockRequest, RenewLockResponse,
        UpdateLockAclRequest, UpdateLockAclResponse, UserLockInfo, UserLocksResponse,
    },
    store::{AcquireLockOptions, AcquireLockOutcome, LockStore},
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use tracing::info;
use uuid::Uuid;

fn renew_after_ms(expires_at: chrono::DateTime<chrono::Utc>) -> u64 {
    retry_after_ms(expires_at) / 2
}

fn retry_after_ms(expires_at: chrono::DateTime<chrono::Utc>) -> u64 {
    (expires_at - chrono::Utc::now()).num_milliseconds().max(0) as u64
}

fn ensure_namespace_access(state: &crate::AppState, user_id: Uuid, lock_name: &str) -> Result<()> {
    if crate::elections::is_reserved_lock_name(lock_name) {
        return Err(AppError::Forbidden(
            "The __election namespace is reserved for public leader elections".to_string(),
        ));
    }

    if user_id == Uuid::nil() {
        return Ok(());
    }

    if let Some(namespace) = state.auth_service.get_user_namespace(user_id)? {
        let required_prefix = format!("{}.", namespace);
        if !lock_name.starts_with(&required_prefix) {
            return Err(AppError::Forbidden(format!(
                "lock '{}' is outside namespace '{}'",
                lock_name, namespace
            )));
        }
    }

    Ok(())
}

fn ensure_prefix_access(
    state: &crate::AppState,
    user_id: Uuid,
    prefix: Option<&str>,
) -> Result<()> {
    if user_id == Uuid::nil() {
        return Ok(());
    }

    if let Some(namespace) = state.auth_service.get_user_namespace(user_id)? {
        let required_prefix = format!("{}.", namespace);
        match prefix {
            Some(prefix) if prefix.starts_with(&required_prefix) => Ok(()),
            Some(prefix) => Err(AppError::Forbidden(format!(
                "prefix '{}' is outside namespace '{}'",
                prefix, namespace
            ))),
            None => Ok(()),
        }
    } else {
        Ok(())
    }
}

fn effective_list_prefix(
    state: &crate::AppState,
    user_id: Uuid,
    requested: Option<String>,
) -> Result<Option<String>> {
    ensure_prefix_access(state, user_id, requested.as_deref())?;
    if requested.is_some() || user_id == Uuid::nil() {
        return Ok(requested);
    }

    Ok(state
        .auth_service
        .get_user_namespace(user_id)?
        .map(|namespace| format!("{namespace}.")))
}

/// Selects the internal authority principal for a lock acquisition.
///
/// Ephemeral locks are owned by their session because session teardown must
/// revoke them. A non-ephemeral lock may still retain `session_id` as lifecycle
/// correlation, but its durable mutation and quota owner is the authenticated
/// user so ending that session cannot orphan the lease.
fn holder_id_for_request(user_id: Uuid, session_id: Option<Uuid>, ephemeral: bool) -> Uuid {
    if ephemeral {
        session_id.unwrap_or(user_id)
    } else {
        user_id
    }
}

/// Derives a stable, UUID-shaped public correlation identity for a lock holder.
///
/// The internal holder UUID may be an actionable session capability. Public
/// lock responses must expose only this domain-separated one-way pseudonym;
/// mutation and session endpoints continue to require the real internal UUID.
pub(crate) fn public_holder_id(internal_holder_id: Uuid) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"octostore.public-lock-holder.v1\0");
    hasher.update(internal_holder_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);

    // Mark the digest as an RFC 9562 custom UUID (version 8) while retaining
    // UUID-shaped wire compatibility for existing clients.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut public_id = Uuid::from_bytes(bytes);
    if public_id == internal_holder_id {
        // Make non-disclosure absolute even in the vanishingly unlikely event
        // that the truncated digest is a fixed point of the input UUID.
        bytes[0] ^= 1;
        public_id = Uuid::from_bytes(bytes);
    }
    public_id
}

fn holder_for_mutation(state: &crate::AppState, user_id: Uuid, name: &str) -> (Uuid, Option<Uuid>) {
    let Some(lock) = state.lock_handlers.store.get_lock(name) else {
        return (user_id, None);
    };
    if lock.holder_id == user_id {
        return (user_id, None);
    }
    let Some(session_id) = lock.session_id else {
        return (user_id, None);
    };
    if lock.ephemeral && lock.holder_id == session_id {
        (session_id, Some(session_id))
    } else {
        (user_id, None)
    }
}

fn user_owns_lock_instance(
    state: &crate::AppState,
    user_id: Uuid,
    lock: &crate::models::Lock,
) -> bool {
    if lock.holder_id == user_id {
        return true;
    }
    lock.session_id.is_some_and(|session_id| {
        lock.holder_id == session_id
            && state
                .session_store
                .get_session(session_id)
                .is_some_and(|session| session.user_id == user_id && !session.is_expired())
    })
}

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
        .map(|principal| {
            let principal = principal.trim();
            if let Some(username) = principal.strip_prefix("user:") {
                format!("user:{}", username.to_lowercase())
            } else {
                principal.to_string()
            }
        })
        .collect();
    acquire.sort();
    acquire.dedup();
    LockAcl { acquire }
}

fn validate_acl(acl: &LockAcl) -> Result<()> {
    if acl.acquire.is_empty() {
        return Err(AppError::InvalidInput(
            "acl.acquire must not be empty".to_string(),
        ));
    }
    if acl.acquire.len() > 100 {
        return Err(AppError::InvalidInput(
            "acl.acquire cannot contain more than 100 principals".to_string(),
        ));
    }

    for principal in &acl.acquire {
        let p = principal.trim();
        if p.len() > 256 {
            return Err(AppError::InvalidInput(
                "acl principals cannot exceed 256 characters".to_string(),
            ));
        }

        let valid = if let Some(username) = p.strip_prefix("user:") {
            !username.is_empty()
                && username.len() <= 64
                && username
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        } else if let Some(token) = p.strip_prefix("token:") {
            !token.is_empty()
        } else {
            false
        };
        if !valid {
            return Err(AppError::InvalidInput(
                "acl principals must use user:<github_username> or token:<token>".to_string(),
            ));
        }
    }
    Ok(())
}

fn redact_acl(acl: &LockAcl) -> LockAcl {
    LockAcl {
        acquire: acl
            .acquire
            .iter()
            .map(|principal| {
                if principal.starts_with("token:") {
                    "token:[redacted]".to_string()
                } else {
                    principal.clone()
                }
            })
            .collect(),
    }
}

fn caller_in_acl(acl: &LockAcl, username: Option<&str>, token: Option<&str>) -> bool {
    acl.acquire.iter().any(|principal| {
        if let Some(rest) = principal.strip_prefix("user:") {
            return username
                .map(|u| u.eq_ignore_ascii_case(rest))
                .unwrap_or(false);
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
    ensure_namespace_access(&state, user_id, &name)?;

    // Validate TTL
    let ttl_seconds = req.ttl_seconds.unwrap_or(60);
    validate_ttl(ttl_seconds)?;

    // Validate metadata
    validate_metadata(&req.metadata)?;

    let ephemeral = req.ephemeral.unwrap_or(false);
    let lock_delay_seconds = req.lock_delay_seconds.unwrap_or(0);
    if lock_delay_seconds > 30 {
        return Err(AppError::InvalidInput(
            "lock_delay_seconds must be between 0 and 30".to_string(),
        ));
    }

    // Ephemeral locks require a session_id
    if ephemeral && req.session_id.is_none() {
        return Err(AppError::InvalidInput(
            "ephemeral locks require a session_id".to_string(),
        ));
    }

    let holder_id = holder_id_for_request(user_id, req.session_id, ephemeral);

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
    if let Some((available_at, delay)) = state.lock_handlers.store.check_cooling(&name)? {
        return Ok((
            StatusCode::CONFLICT,
            Json(AcquireLockResponse::Delayed {
                available_at,
                lock_delay_seconds: delay,
                retry_after_ms: retry_after_ms(available_at),
            }),
        ));
    }

    let acquire = || {
        state
            .lock_handlers
            .store
            .acquire_lock_outcome_with_principal_limit(
                name.clone(),
                holder_id,
                AcquireLockOptions::new(ttl_seconds)
                    .with_metadata(req.metadata.clone())
                    .with_session_id(req.session_id)
                    .ephemeral(ephemeral)
                    .with_lock_delay_seconds(lock_delay_seconds)
                    .with_acl_context(existing_acl.clone(), requested_acl.clone()),
                100,
                |lock| {
                    lock.holder_id == user_id
                        || lock.session_id.is_some_and(|session_id| {
                            state
                                .session_store
                                .get_session(session_id)
                                .is_some_and(|session| {
                                    session.user_id == user_id && !session.is_expired()
                                })
                        })
                },
            )
    };
    let acquisition = match req.session_id {
        Some(session_id) => state
            .session_store
            .with_active_session(session_id, user_id, |_| acquire()),
        None => acquire(),
    };

    match acquisition {
        Ok(AcquireLockOutcome::Acquired(lock)) => {
            state.metrics.record_lock_operation("acquire");
            info!("Lock acquired: {} by user {}", name, user_id);
            Ok((
                StatusCode::OK,
                Json(AcquireLockResponse::Acquired {
                    lease_id: lock.lease_id,
                    fencing_token: lock.fencing_token,
                    expires_at: lock.expires_at,
                    renew_after_ms: renew_after_ms(lock.expires_at),
                    metadata: lock.metadata,
                }),
            ))
        }
        Ok(AcquireLockOutcome::Held(lock)) => Ok((
            StatusCode::OK,
            Json(AcquireLockResponse::Held {
                holder_id: public_holder_id(lock.holder_id),
                expires_at: lock.expires_at,
                retry_after_ms: retry_after_ms(lock.expires_at),
                metadata: lock.metadata,
            }),
        )),
        Ok(AcquireLockOutcome::Delayed {
            available_at,
            lock_delay_seconds,
        }) => Ok((
            StatusCode::CONFLICT,
            Json(AcquireLockResponse::Delayed {
                available_at,
                lock_delay_seconds,
                retry_after_ms: retry_after_ms(available_at),
            }),
        )),
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
    ensure_namespace_access(&state, user_id, &name)?;
    let (holder_id, session_id) = holder_for_mutation(&state, user_id, &name);
    let release = || {
        state
            .lock_handlers
            .store
            .release_lock(&name, req.lease_id, holder_id)
    };
    match session_id {
        Some(session_id) => state
            .session_store
            .with_active_session(session_id, user_id, |_| release())?,
        None => release()?,
    }

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
    ensure_namespace_access(&state, user_id, &name)?;
    let (holder_id, session_id) = holder_for_mutation(&state, user_id, &name);

    let ttl_seconds = req.ttl_seconds.unwrap_or(60);
    validate_ttl(ttl_seconds)?;

    let renew = || {
        state
            .lock_handlers
            .store
            .renew_lock(&name, req.lease_id, holder_id, ttl_seconds)
    };
    let renewed_lock = match session_id {
        Some(session_id) => state
            .session_store
            .with_active_session(session_id, user_id, |_| renew())?,
        None => renew()?,
    };

    info!("Lock renewed: {} by user {}", name, user_id);
    Ok(Json(RenewLockResponse {
        lease_id: req.lease_id,
        expires_at: renewed_lock.expires_at,
        renew_after_ms: renew_after_ms(renewed_lock.expires_at),
    }))
}

pub async fn get_lock_status(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<LockStatusResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;

    validate_lock_name(&name)?;
    ensure_namespace_access(&state, user_id, &name)?;

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
                acl: acl.as_ref().map(redact_acl),
            }))
        } else {
            Ok(Json(LockStatusResponse {
                name: name.clone(),
                status: "held".to_string(),
                holder_id: Some(public_holder_id(lock.holder_id)),
                fencing_token: lock.fencing_token,
                expires_at: Some(lock.expires_at),
                metadata: lock.metadata.clone(),
                acl: acl.as_ref().map(redact_acl),
            }))
        }
    } else {
        // Lock doesn't exist, it's free
        // We need to determine what fencing token would be used next
        let next_fencing_token = state.lock_handlers.store.get_fencing_counter();
        Ok(Json(LockStatusResponse {
            name: name.clone(),
            status: "free".to_string(),
            holder_id: None,
            fencing_token: next_fencing_token,
            expires_at: None,
            metadata: None,
            acl: acl.as_ref().map(redact_acl),
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

    if is_admin {
        state.lock_handlers.store.set_lock_acl(&name, &acl)?;
    } else {
        let expected = state
            .lock_handlers
            .store
            .get_lock(&name)
            .ok_or(AppError::LockNotFound { name: name.clone() })?;

        if !user_owns_lock_instance(&state, user_id, &expected) || expected.is_expired() {
            return Err(AppError::Forbidden(
                "only current lock holder or admin can update ACL".to_string(),
            ));
        }

        if expected.holder_id == user_id {
            state
                .lock_handlers
                .store
                .set_lock_acl_for_instance(&name, &expected, &acl)?;
        } else {
            let session_id = expected.session_id.ok_or_else(|| {
                AppError::Forbidden("only current lock holder or admin can update ACL".to_string())
            })?;
            state
                .session_store
                .with_active_session(session_id, user_id, |_| {
                    state
                        .lock_handlers
                        .store
                        .set_lock_acl_for_instance(&name, &expected, &acl)
                })?;
        }
    }
    Ok(Json(UpdateLockAclResponse {
        name,
        acl: redact_acl(&acl),
    }))
}

/// Watches a lock for real-time state changes via Server-Sent Events (SSE).
fn sanitized_lock_watch_stream(
    rx: tokio::sync::broadcast::Receiver<crate::models::LockEvent>,
) -> impl Stream<Item = std::result::Result<Event, Infallible>> {
    tokio_stream::wrappers::BroadcastStream::new(rx)
        .map(|message| {
            message
                .ok()
                .map(LockWatchEvent::from)
                .and_then(|event| serde_json::to_string(&event).ok())
                .map(|json| Ok(Event::default().data(json)))
        })
        .take_while(|event| futures::future::ready(event.is_some()))
        .map(|event| event.expect("watch event checked by take_while"))
}

fn current_lock_watch_event(store: &LockStore, name: &str) -> LockWatchEvent {
    let current = store.get_lock(name).filter(|lock| !lock.is_expired());
    LockWatchEvent {
        event: if current.is_some() {
            LockEventType::Acquired
        } else {
            LockEventType::Released
        },
        lock_name: name.to_string(),
        fencing_token: current.as_ref().map(|lock| lock.fencing_token),
        expires_at: current.as_ref().map(|lock| lock.expires_at),
        observed_at: chrono::Utc::now(),
    }
}

fn serialized_lock_watch_event(event: LockWatchEvent) -> Result<Event> {
    Ok(Event::default().data(serde_json::to_string(&event)?))
}

/// Watches a lock for real-time state changes via Server-Sent Events (SSE).
pub async fn watch_lock(
    Path(name): Path<String>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    // Authenticate the user
    let user_id = state.auth_service.authenticate(&headers)?;

    validate_lock_name(&name)?;
    ensure_namespace_access(&state, user_id, &name)?;

    // Subscribe first, then snapshot. Any mutation racing with the snapshot is
    // retained in the receiver, so the stream cannot miss the only transition.
    let rx = state.lock_handlers.store.watch_lock(&name)?;
    let initial =
        serialized_lock_watch_event(current_lock_watch_event(&state.lock_handlers.store, &name))?;

    // A lagged or unserializable signal closes the stream. Clients reconcile
    // through GET before reconnecting, so silently skipping would be unsafe.
    let stream = stream::once(async move { Ok(initial) }).chain(sanitized_lock_watch_stream(rx));

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
    let user_id = state.auth_service.authenticate(&headers)?;
    let prefix = effective_list_prefix(&state, user_id, query.prefix)?;

    let locks = state.lock_handlers.store.list_locks(prefix.as_deref());
    let lock_responses: Vec<LockStatusResponse> = locks
        .into_iter()
        .filter(|lock| !lock.is_expired() && !crate::elections::is_reserved_lock_name(&lock.name))
        .map(|lock| LockStatusResponse {
            name: lock.name,
            status: "held".to_string(),
            holder_id: Some(public_holder_id(lock.holder_id)),
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
        prefix,
    }))
}

#[allow(dead_code)]
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
    use rusqlite::Connection;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

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

        handlers
            .store
            .acquire_lock("shared-test".into(), user_id, AcquireLockOptions::new(60))
            .unwrap();
        assert_eq!(
            cloned.store.count_user_locks(user_id),
            1,
            "cloned handlers should see locks created through the original"
        );
    }

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn assert_renew_guidance_matches_expiry(response: &Value) {
        let expires_at = chrono::DateTime::parse_from_rfc3339(
            response["expires_at"]
                .as_str()
                .expect("response includes expires_at"),
        )
        .unwrap()
        .with_timezone(&chrono::Utc);
        let current_half_remaining =
            (expires_at - chrono::Utc::now()).num_milliseconds().max(0) as u64 / 2;
        let guidance = response["renew_after_ms"]
            .as_u64()
            .expect("response includes renew_after_ms");
        assert!(guidance >= current_half_remaining);
        assert!(
            guidance - current_half_remaining <= 2_000,
            "renewal guidance {guidance} must be half of actual remaining lease {current_half_remaining}"
        );
    }

    async fn test_app() -> (axum::Router, NamedTempFile) {
        let (router, temp_file, _, _) = test_app_with_store().await;
        (router, temp_file)
    }

    async fn test_app_with_store() -> (axum::Router, NamedTempFile, LockStore, Uuid) {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();

        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: db_path,
github_client_id: None,
github_client_secret: None,
github_redirect_uri: "http://localhost:3000/callback".to_string(),
oauth_api_base_url: None,
oauth_dashboard_url: None,
            admin_key: Some("test_admin_key".to_string()),
            static_tokens: Some(
                "testuser:testtoken,user2:token2,caseuser:CaseSensitiveToken,caseuserlower:casesensitivetoken"
                    .to_string(),
            ),
            static_tokens_file: None,
            admin_username: None,
            local_registration_enabled: false,
            public_elections_enabled: true,
            max_public_elections: 100,
            public_election_requests_per_minute: 600,
            public_election_watch_streams_global: 100,
            public_election_watch_streams_per_client: 8,
            public_election_watch_max_seconds: 900,
        };

        // Share one DbConn between both services (#19)
        let db: crate::store::DbConn = std::sync::Arc::new(std::sync::Mutex::new(
            Connection::open(&config.database_url).unwrap(),
        ));
        let auth_service = AuthService::new(config.clone(), db.clone()).unwrap();
        auth_service.seed_static_tokens().unwrap();
        let test_user_id = {
            let conn = auth_service.db.lock().unwrap();
            conn.execute(
                "UPDATE users SET namespace = 'team-a' WHERE token = 'testtoken'",
                [],
            )
            .unwrap();
            let user_id: String = conn
                .query_row(
                    "SELECT id FROM users WHERE token = 'testtoken'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            Uuid::parse_str(&user_id).unwrap()
        };
        let lock_store = LockStore::new(db.clone(), 0).unwrap();
        let lock_handlers = LockHandlers::new(lock_store.clone());
        let session_store = crate::sessions::SessionStore::new(db.clone()).unwrap();
        let webhook_store = crate::webhooks::WebhookStore::new(db).unwrap();

        let app_state = crate::app::AppState {
            lock_handlers,
            auth_service,
            config: config.clone(),
            metrics: crate::metrics::Metrics::new(),
            public_election_rate_limiter: crate::rate_limit::PublicElectionRateLimiter::new(600),
            public_election_watch_limiter: crate::rate_limit::PublicElectionWatchLimiter::new(
                100, 8,
            ),
            session_store,
            webhook_store,
        };

        let router = axum::Router::new()
            .route(
                "/sessions",
                axum::routing::post(crate::sessions::create_session),
            )
            .route(
                "/sessions/:id/keepalive",
                axum::routing::post(crate::sessions::keepalive),
            )
            .route(
                "/sessions/:id",
                axum::routing::get(crate::sessions::get_session_status)
                    .delete(crate::sessions::terminate_session),
            )
            .route("/locks/:name/acquire", axum::routing::post(acquire_lock))
            .route("/locks/:name/acl", axum::routing::put(update_lock_acl))
            .route("/locks/:name/release", axum::routing::post(release_lock))
            .route("/locks/:name/renew", axum::routing::post(renew_lock))
            .route("/locks/:name/watch", axum::routing::get(watch_lock))
            .route("/locks/:name", axum::routing::get(get_lock_status))
            .route("/locks", axum::routing::get(list_locks))
            .with_state(app_state);

        (router, temp_file, lock_store, test_user_id)
    }

    async fn create_test_session(app: &axum::Router) -> Uuid {
        let response = app
            .clone()
            .oneshot(
                Request::post("/sessions")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        Uuid::parse_str(body["session_id"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_acquire_status_release_roundtrip() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        let lease_id = res["lease_id"].as_str().unwrap().to_string();
        assert_eq!(res["status"], "acquired");
        assert_renew_guidance_matches_expiry(&res);

        // 2. Get Status
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(res["status"], "held");

        // 3. Release
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock/release")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id": lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_idempotent_reacquire() {
        let (app, _tmp) = test_app().await;

        let req = Request::builder()
            .uri("/locks/team-a.test-lock/acquire")
            .method("POST")
            .header("authorization", "Bearer testtoken")
            .header("content-type", "application/json")
            .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
            .unwrap();

        let response = app.clone().oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let res1: Value = serde_json::from_slice(&body).unwrap();

        let req2 = Request::builder()
            .uri("/locks/team-a.test-lock/acquire")
            .method("POST")
            .header("authorization", "Bearer testtoken")
            .header("content-type", "application/json")
            .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
            .unwrap();

        let response2 = app.oneshot(req2).await.unwrap();
        let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
            .await
            .unwrap();
        let res2: Value = serde_json::from_slice(&body2).unwrap();

        assert_eq!(
            res1["lease_id"], res2["lease_id"],
            "Idempotent re-acquire should return same lease_id"
        );
    }

    #[tokio::test]
    async fn same_holder_reacquire_returns_the_persisted_metadata_snapshot() {
        let (app, temp_file, store, _) = test_app_with_store().await;
        let path = temp_file.path().to_path_buf();

        let first = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.metadata/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":300,"metadata":"persisted"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let reacquired = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.metadata/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":300,"metadata":"replacement-request"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reacquired.status(), StatusCode::OK);
        let body = axum::body::to_bytes(reacquired.into_body(), usize::MAX)
            .await
            .unwrap();
        let reacquired: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reacquired["status"], "acquired");
        assert_eq!(reacquired["metadata"], "persisted");

        let status = app
            .oneshot(
                Request::get("/locks/team-a.metadata")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let body = axum::body::to_bytes(status.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["metadata"], "persisted");

        drop(store);
        let db: crate::store::DbConn =
            std::sync::Arc::new(std::sync::Mutex::new(Connection::open(path).unwrap()));
        let restored = LockStore::new(db, 0)
            .unwrap()
            .get_lock("team-a.metadata")
            .expect("persisted lock should survive restart");
        assert_eq!(restored.metadata.as_deref(), Some("persisted"));
    }

    #[tokio::test]
    async fn same_holder_reacquire_at_limit_renews_and_uses_actual_expiry_guidance() {
        let (app, _tmp, store, user_id) = test_app_with_store().await;
        let first = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.at-limit/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":300}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();
        let first: Value = serde_json::from_slice(&first).unwrap();

        for index in 0..99 {
            store
                .acquire_lock(
                    format!("team-a.limit-fill-{index}"),
                    user_id,
                    AcquireLockOptions::new(300),
                )
                .unwrap();
        }
        assert_eq!(store.count_user_locks(user_id), 100);

        // A shorter requested TTL must not shorten the lease, and guidance is
        // based on the retained ~300-second expiry rather than the 30-second request.
        let shorter = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.at-limit/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":30}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shorter.status(), StatusCode::OK);
        let shorter = axum::body::to_bytes(shorter.into_body(), usize::MAX)
            .await
            .unwrap();
        let shorter: Value = serde_json::from_slice(&shorter).unwrap();
        assert_eq!(shorter["lease_id"], first["lease_id"]);
        assert_eq!(shorter["fencing_token"], first["fencing_token"]);
        assert_eq!(shorter["expires_at"], first["expires_at"]);
        assert_renew_guidance_matches_expiry(&shorter);
        assert!(shorter["renew_after_ms"].as_u64().unwrap() > 100_000);

        // A longer request must pass the lock-limit gate and atomically renew
        // the same lease instead of returning the stale pre-renewal snapshot.
        let renewal_floor = chrono::Utc::now() + chrono::Duration::seconds(600);
        let longer = app
            .oneshot(
                Request::post("/locks/team-a.at-limit/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":600}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(longer.status(), StatusCode::OK);
        let longer = axum::body::to_bytes(longer.into_body(), usize::MAX)
            .await
            .unwrap();
        let longer: Value = serde_json::from_slice(&longer).unwrap();
        let longer_expiry =
            chrono::DateTime::parse_from_rfc3339(longer["expires_at"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
        assert_eq!(longer["lease_id"], first["lease_id"]);
        assert_eq!(longer["fencing_token"], first["fencing_token"]);
        assert!(longer_expiry >= renewal_floor);
        assert_renew_guidance_matches_expiry(&longer);
        assert!(longer["renew_after_ms"].as_u64().unwrap() > 250_000);
    }

    #[tokio::test]
    async fn test_release_wrong_lease() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. Release with wrong lease
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock/release")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id": Uuid::new_v4()}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_acquire_expired_lock() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire with 1s TTL
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 1}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // 2. Wait 2s
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // 3. Re-acquire should succeed (different lease_id)
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn expired_lock_enters_delay_even_before_periodic_cleanup() {
        let (app, _tmp, store, _) = test_app_with_store().await;
        let response = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.expired-delay/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":1,"lock_delay_seconds":3}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let acquired: Value = serde_json::from_slice(&body).unwrap();
        let expires_at = chrono::DateTime::parse_from_rfc3339(
            acquired["expires_at"].as_str().expect("acquire expiry"),
        )
        .unwrap()
        .with_timezone(&chrono::Utc);
        while chrono::Utc::now() <= expires_at {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let response = app
            .oneshot(
                Request::post("/locks/team-a.expired-delay/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let delayed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(delayed["status"], "delayed");
        assert_eq!(delayed["lock_delay_seconds"], 3);
        assert!(delayed["retry_after_ms"].as_u64().unwrap() > 0);
        assert!(store.get_lock("team-a.expired-delay").is_none());
    }

    #[tokio::test]
    async fn acquire_rejects_lock_delay_above_documented_maximum() {
        let (app, _tmp) = test_app().await;
        let response = app
            .oneshot(
                Request::post("/locks/team-a.invalid-delay/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":60,"lock_delay_seconds":300}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "invalid_input");
        assert!(error["details"]
            .as_str()
            .unwrap()
            .contains("between 0 and 30"));
    }

    #[tokio::test]
    async fn test_acquire_renew_release_lifecycle() {
        let (app, _tmp) = test_app().await;

        // 1. Acquire
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.renew-test/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let acquired: Value = serde_json::from_slice(&body).unwrap();
        let lease_id = acquired["lease_id"].as_str().unwrap().to_string();

        // 2. Renew
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.renew-test/renew")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"lease_id": lease_id, "ttl_seconds": 120}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let renewed: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            renewed["expires_at"].is_string(),
            "renew should return new expires_at"
        );
        assert_renew_guidance_matches_expiry(&renewed);

        // 3. Release
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.renew-test/release")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id": lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_acquire_held_by_another_user() {
        let (app, _tmp) = test_app().await;

        // user1 acquires the lock
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.contested-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.contested-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer token2")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let held: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(held["status"], "held");
        assert!(held["retry_after_ms"]
            .as_u64()
            .is_some_and(|value| value > 0));

        // user2 is unscoped and can still inspect the held lock
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.contested-lock")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let res: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(res["status"], "held");
    }

    #[tokio::test]
    async fn delayed_acquire_and_stale_lease_have_machine_guidance() {
        let (app, _tmp) = test_app().await;
        let acquired = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.delayed-lock/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":60,"lock_delay_seconds":1}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let acquired = axum::body::to_bytes(acquired.into_body(), usize::MAX)
            .await
            .unwrap();
        let acquired: Value = serde_json::from_slice(&acquired).unwrap();
        let lease_id = acquired["lease_id"].as_str().unwrap();

        let released = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.delayed-lock/release")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id":lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(released.status(), StatusCode::OK);

        let delayed = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.delayed-lock/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delayed.status(), StatusCode::CONFLICT);
        let delayed = axum::body::to_bytes(delayed.into_body(), usize::MAX)
            .await
            .unwrap();
        let delayed: Value = serde_json::from_slice(&delayed).unwrap();
        assert_eq!(delayed["status"], "delayed");
        assert!(delayed["retry_after_ms"]
            .as_u64()
            .is_some_and(|value| value > 0));

        let stale = app
            .oneshot(
                Request::post("/locks/team-a.delayed-lock/renew")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id":lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::NOT_FOUND);
        let stale = axum::body::to_bytes(stale.into_body(), usize::MAX)
            .await
            .unwrap();
        let stale: Value = serde_json::from_slice(&stale).unwrap();
        assert_eq!(stale["code"], "lease_not_current");
    }

    #[tokio::test]
    async fn public_holder_pseudonym_cannot_inspect_or_terminate_same_token_session() {
        let (app, _tmp) = test_app().await;
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/sessions")
                        .header("authorization", "Bearer testtoken")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            sessions.push(
                serde_json::from_slice::<Value>(&body).unwrap()["session_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }

        let first = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.session-owned/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":60,"session_id":sessions[0],"ephemeral":true})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let first = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();
        let first: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(first["status"], "acquired");
        let lease_id = first["lease_id"].as_str().unwrap().to_string();

        let second = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.session-owned/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds":60,"session_id":sessions[1],"ephemeral":true})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = axum::body::to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap();
        let second: Value = serde_json::from_slice(&second).unwrap();
        assert_eq!(second["status"], "held");
        let public_holder_id = second["holder_id"].as_str().unwrap().to_string();
        Uuid::parse_str(&public_holder_id).expect("public holder remains UUID-shaped");
        assert_ne!(public_holder_id, sessions[0]);
        assert_ne!(public_holder_id, sessions[1]);

        let status = app
            .clone()
            .oneshot(
                Request::get("/locks/team-a.session-owned")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = axum::body::to_bytes(status.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: Value = serde_json::from_slice(&status).unwrap();
        assert_eq!(status["holder_id"], public_holder_id);

        let listed = app
            .clone()
            .oneshot(
                Request::get("/locks?prefix=team-a.session-owned")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let listed = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .unwrap();
        let listed: Value = serde_json::from_slice(&listed).unwrap();
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["locks"][0]["holder_id"], public_holder_id);

        // The second session is a same-token sibling. Even with that token,
        // the public holder correlation ID is not an actionable session ID.
        let inspect_pseudonym = app
            .clone()
            .oneshot(
                Request::get(format!("/sessions/{public_holder_id}"))
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(inspect_pseudonym.status(), StatusCode::NOT_FOUND);

        let terminate_pseudonym = app
            .clone()
            .oneshot(
                Request::delete(format!("/sessions/{public_holder_id}"))
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(terminate_pseudonym.status(), StatusCode::NOT_FOUND);

        let real_session = app
            .clone()
            .oneshot(
                Request::get(format!("/sessions/{}", sessions[0]))
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(real_session.status(), StatusCode::OK);
        let real_session = axum::body::to_bytes(real_session.into_body(), usize::MAX)
            .await
            .unwrap();
        let real_session: Value = serde_json::from_slice(&real_session).unwrap();
        assert_eq!(real_session["session_id"], sessions[0]);
        assert_eq!(real_session["lock_count"], 1);

        let renewed = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.session-owned/renew")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id":lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(renewed.status(), StatusCode::OK);

        let released = app
            .oneshot(
                Request::post("/locks/team-a.session-owned/release")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id":lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(released.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn durable_session_correlated_lock_remains_user_managed_after_teardown() {
        let (app, _tmp, store, user_id) = test_app_with_store().await;
        let session_id = create_test_session(&app).await;

        let acquired = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.durable-session/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "ttl_seconds": 300,
                            "session_id": session_id,
                            "ephemeral": false
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(acquired.status(), StatusCode::OK);
        let acquired = axum::body::to_bytes(acquired.into_body(), usize::MAX)
            .await
            .unwrap();
        let acquired: Value = serde_json::from_slice(&acquired).unwrap();
        let lease_id = acquired["lease_id"].as_str().unwrap().to_string();

        let stored = store.get_lock("team-a.durable-session").unwrap();
        assert_eq!(stored.holder_id, user_id);
        assert_eq!(stored.session_id, Some(session_id));
        assert!(!stored.ephemeral);

        let terminated = app
            .clone()
            .oneshot(
                Request::delete(format!("/sessions/{session_id}"))
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(terminated.status(), StatusCode::NO_CONTENT);
        assert!(store.get_lock("team-a.durable-session").is_some());

        let acl_updated = app
            .clone()
            .oneshot(
                Request::put("/locks/team-a.durable-session/acl")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"acl":{"acquire":["user:testuser"]}}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(acl_updated.status(), StatusCode::OK);

        let renewed = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.durable-session/renew")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"lease_id":lease_id,"ttl_seconds":600}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(renewed.status(), StatusCode::OK);

        let released = app
            .oneshot(
                Request::post("/locks/team-a.durable-session/release")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id":lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(released.status(), StatusCode::OK);
        assert!(store.get_lock("team-a.durable-session").is_none());
    }

    #[tokio::test]
    async fn durable_session_correlated_lock_remains_in_user_quota_after_teardown() {
        let (app, _tmp, store, user_id) = test_app_with_store().await;
        let session_id = create_test_session(&app).await;

        let acquired = app
            .clone()
            .oneshot(
                Request::post("/locks/team-a.durable-quota/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "ttl_seconds": 300,
                            "session_id": session_id,
                            "ephemeral": false
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(acquired.status(), StatusCode::OK);

        let terminated = app
            .clone()
            .oneshot(
                Request::delete(format!("/sessions/{session_id}"))
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(terminated.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.count_user_locks(user_id), 1);

        for index in 0..99 {
            store
                .acquire_lock(
                    format!("team-a.durable-quota-fill-{index}"),
                    user_id,
                    AcquireLockOptions::new(300),
                )
                .unwrap();
        }
        assert_eq!(store.count_user_locks(user_id), 100);

        let rejected = app
            .oneshot(
                Request::post("/locks/team-a.durable-quota-overflow/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":300}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        let rejected = axum::body::to_bytes(rejected.into_body(), usize::MAX)
            .await
            .unwrap();
        let rejected: Value = serde_json::from_slice(&rejected).unwrap();
        assert_eq!(rejected["code"], "lock_limit_exceeded");
    }

    #[tokio::test]
    async fn test_scoped_token_only_acquires_in_namespace() {
        let (app, _tmp) = test_app().await;

        let forbidden = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-b.worker/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let allowed = app
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.worker/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_scoped_prefix_query_rejected_outside_namespace() {
        let (app, _tmp) = test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks?prefix=team-b")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn scoped_list_without_prefix_defaults_to_caller_namespace() {
        let (app, _tmp) = test_app().await;
        for (name, token) in [("team-a.visible", "testtoken"), ("team-b.hidden", "token2")] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("/locks/{name}/acquire"))
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let response = app
            .oneshot(
                Request::get("/locks")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["prefix"], "team-a.");
        assert_eq!(body["total"], 1);
        assert_eq!(body["locks"][0]["name"], "team-a.visible");
        assert!(!body.to_string().contains("team-b.hidden"));
    }

    #[tokio::test]
    async fn lock_watch_never_exposes_same_token_session_or_lease_capabilities() {
        let (app, _tmp) = test_app().await;
        let session = app
            .clone()
            .oneshot(
                Request::post("/sessions")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let session = axum::body::to_bytes(session.into_body(), usize::MAX)
            .await
            .unwrap();
        let session: Value = serde_json::from_slice(&session).unwrap();
        let session_id = session["session_id"].as_str().unwrap().to_string();

        let watch = app
            .clone()
            .oneshot(
                Request::get("/locks/team-a.watched/watch")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(watch.status(), StatusCode::OK);
        let mut watch_body = watch.into_body().into_data_stream();

        let initial = tokio::time::timeout(std::time::Duration::from_secs(1), watch_body.next())
            .await
            .expect("initial lock snapshot should arrive")
            .expect("watch should remain open")
            .expect("initial lock snapshot should be readable");
        let initial = String::from_utf8(initial.to_vec()).unwrap();
        assert!(initial.contains("\"event\":\"released\""));
        assert!(initial.contains("\"lock_name\":\"team-a.watched\""));

        let acquired = app
            .oneshot(
                Request::post("/locks/team-a.watched/acquire")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "ttl_seconds":60,
                            "session_id":session_id,
                            "ephemeral":true,
                            "metadata":"not-for-watchers"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let acquired = axum::body::to_bytes(acquired.into_body(), usize::MAX)
            .await
            .unwrap();
        let acquired: Value = serde_json::from_slice(&acquired).unwrap();
        let lease_id = acquired["lease_id"].as_str().unwrap();

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), watch_body.next())
            .await
            .expect("watch event should arrive")
            .expect("watch should remain open")
            .expect("watch frame should be readable");
        let frame = String::from_utf8(frame.to_vec()).unwrap();
        assert!(frame.contains("\"event\":\"acquired\""));
        assert!(frame.contains("\"fencing_token\":"));
        for forbidden in [
            "lease_id",
            "session_id",
            "holder_id",
            "metadata",
            lease_id,
            session_id.as_str(),
            "not-for-watchers",
        ] {
            assert!(
                !frame.contains(forbidden),
                "watch leaked {forbidden}: {frame}"
            );
        }
    }

    #[tokio::test]
    async fn lagged_lock_watch_closes_instead_of_hiding_missed_events() {
        let (handlers, _tmp) = create_test_handlers();
        let receiver = handlers.store.watch_lock("lagged-watch").unwrap();
        let holder = Uuid::new_v4();
        for _ in 0..101 {
            let (lease, _, _) = handlers
                .store
                .acquire_lock(
                    "lagged-watch".to_string(),
                    holder,
                    AcquireLockOptions::new(60),
                )
                .unwrap();
            handlers
                .store
                .release_lock("lagged-watch", lease, holder)
                .unwrap();
        }
        let mut stream = Box::pin(sanitized_lock_watch_stream(receiver));
        assert!(stream.next().await.is_none(), "lagged stream must close");
    }

    #[tokio::test]
    async fn session_status_is_indistinguishable_across_users() {
        let (app, _tmp) = test_app().await;
        let created = app
            .clone()
            .oneshot(
                Request::post("/sessions")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds":60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let created = axum::body::to_bytes(created.into_body(), usize::MAX)
            .await
            .unwrap();
        let created: Value = serde_json::from_slice(&created).unwrap();
        let session_id = created["session_id"].as_str().unwrap();

        let foreign = app
            .clone()
            .oneshot(
                Request::get(format!("/sessions/{session_id}"))
                    .header("authorization", "Bearer token2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(foreign.status(), StatusCode::NOT_FOUND);

        let missing = app
            .oneshot(
                Request::get(format!("/sessions/{}", Uuid::new_v4()))
                    .header("authorization", "Bearer token2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_acl_blocks_non_members() {
        let (app, _tmp) = test_app().await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.acl-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"ttl_seconds": 60, "acl": {"acquire": ["user:testuser"]}})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.acl-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer token2")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_acl_update_requires_holder_or_admin() {
        let (app, _tmp) = test_app().await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.acl-update/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.acl-update/acl")
                    .method("PUT")
                    .header("authorization", "Bearer token2")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"acl": {"acquire": ["user:user2"]}}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.acl-update/acl")
                    .method("PUT")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"acl": {"acquire": ["user:testuser"]}}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let admin_update = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.acl-update/acl")
                    .method("PUT")
                    .header("x-admin-key", "test_admin_key")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"acl": {"acquire": ["token:CaseSensitiveToken"]}}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin_update.status(), StatusCode::OK);
        let body = axum::body::to_bytes(admin_update.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["acl"]["acquire"][0], "token:[redacted]");
        assert!(!body.to_string().contains("CaseSensitiveToken"));
    }

    #[test]
    fn test_acl_normalization_preserves_token_case_and_normalizes_users() {
        let normalized = normalize_acl(&LockAcl {
            acquire: vec![
                " user:Deploy-Bot ".to_string(),
                "token:CaseSensitiveToken".to_string(),
                "user:deploy-bot".to_string(),
            ],
        });

        assert_eq!(
            normalized.acquire,
            vec![
                "token:CaseSensitiveToken".to_string(),
                "user:deploy-bot".to_string()
            ]
        );
    }

    #[test]
    fn test_acl_rejects_too_many_principals() {
        let acl = LockAcl {
            acquire: (0..101).map(|index| format!("user:bot-{index}")).collect(),
        };

        assert!(matches!(validate_acl(&acl), Err(AppError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn test_token_acl_is_case_sensitive_and_redacted_from_status() {
        let (app, _tmp) = test_app().await;

        let acquired = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.token-acl/acquire")
                    .method("POST")
                    .header("authorization", "Bearer CaseSensitiveToken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "ttl_seconds": 60,
                            "acl": {"acquire": ["token:CaseSensitiveToken"]}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(acquired.status(), StatusCode::OK);

        let wrong_case = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.token-acl/acquire")
                    .method("POST")
                    .header("authorization", "Bearer casesensitivetoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_case.status(), StatusCode::FORBIDDEN);

        let status = app
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.token-acl")
                    .header("authorization", "Bearer testtoken")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let body = axum::body::to_bytes(status.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["acl"]["acquire"][0], "token:[redacted]");
        assert!(!body.to_string().contains("CaseSensitiveToken"));
    }

    #[tokio::test]
    async fn test_acl_remains_after_release() {
        let (app, _tmp) = test_app().await;

        let acquired = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.sticky-acl/acquire")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "ttl_seconds": 60,
                            "acl": {"acquire": ["user:testuser"]}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(acquired.status(), StatusCode::OK);
        let body = axum::body::to_bytes(acquired.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let lease_id = body["lease_id"].as_str().unwrap();

        let released = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.sticky-acl/release")
                    .method("POST")
                    .header("authorization", "Bearer testtoken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lease_id": lease_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(released.status(), StatusCode::OK);

        let blocked = app
            .oneshot(
                Request::builder()
                    .uri("/locks/team-a.sticky-acl/acquire")
                    .method("POST")
                    .header("authorization", "Bearer token2")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"ttl_seconds": 60}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    }
}
