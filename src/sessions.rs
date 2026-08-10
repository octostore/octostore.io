use crate::{
    error::{AppError, Result},
    models::{
        CreateSessionRequest, CreateSessionResponse, KeepAliveResponse, Session,
        SessionStatusResponse,
    },
    store::{DbConn, LockStore},
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rusqlite::params;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time;
use tracing::{debug, info, warn};
use uuid::Uuid;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};

const MIN_TTL: u32 = 10;
const MAX_TTL: u32 = 300;
const DEFAULT_TTL: u32 = 60;

#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<DashMap<Uuid, Session>>,
    db: DbConn,
    /// Serializes active-session validation with explicit/expiry teardown.
    /// A session-bound lock acquisition holds this guard until the lock is
    /// durable, so teardown cannot take its one cleanup snapshot too early.
    lifecycle_guard: Arc<Mutex<()>>,
}

impl SessionStore {
    pub fn new(db: DbConn) -> Result<Self> {
        {
            let conn = db.lock().unwrap();
            conn.execute(
                r#"
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    ttl_seconds INTEGER NOT NULL,
                    expires_at TEXT NOT NULL,
                    created_at TEXT NOT NULL
                )
                "#,
                [],
            )?;
        }

        info!("Sessions table initialized");

        let store = Self {
            sessions: Arc::new(DashMap::new()),
            db,
            lifecycle_guard: Arc::new(Mutex::new(())),
        };

        store.load_sessions_from_database()?;

        Ok(store)
    }

    fn load_sessions_from_database(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let now = Utc::now();

        let mut stmt =
            db.prepare("SELECT id, user_id, ttl_seconds, expires_at, created_at FROM sessions")?;

        let rows = stmt.query_map([], |row| {
            let id_str: String = row.get(0)?;
            let user_id_str: String = row.get(1)?;
            let ttl_seconds: u32 = row.get(2)?;
            let expires_at_str: String = row.get(3)?;
            let created_at_str: String = row.get(4)?;

            let id = Uuid::parse_str(&id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let user_id = Uuid::parse_str(&user_id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let expires_at = DateTime::parse_from_rfc3339(&expires_at_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&chrono::Utc);
            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&chrono::Utc);

            Ok(Session {
                id,
                user_id,
                ttl_seconds,
                expires_at,
                created_at,
            })
        })?;

        let mut loaded = 0;
        let mut expired = 0;

        for row in rows {
            let session = row?;
            if session.expires_at > now {
                loaded += 1;
            } else {
                // Keep the expired row in memory and SQLite until checked
                // ephemeral-lock cleanup succeeds. It is durable retry state,
                // not live authority: every authority path still rejects it.
                expired += 1;
            }
            self.sessions.insert(session.id, session);
        }

        info!(
            "Loaded {} active sessions, retained {} expired sessions for checked cleanup",
            loaded, expired
        );

        Ok(())
    }

    pub fn create_session(&self, user_id: Uuid, ttl_seconds: Option<u32>) -> Result<Session> {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        let ttl = clamp_ttl(ttl_seconds);
        let now = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            user_id,
            ttl_seconds: ttl,
            expires_at: now + chrono::Duration::seconds(ttl as i64),
            created_at: now,
        };

        self.save_session_to_database(&session)?;
        self.sessions.insert(session.id, session.clone());

        info!("Session created: {} for user {}", session.id, user_id);
        Ok(session)
    }

    pub fn keepalive(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        ttl_seconds: Option<u32>,
    ) -> Result<DateTime<Utc>> {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        let mut entry = self
            .sessions
            .get_mut(&session_id)
            .ok_or(AppError::SessionNotFound)?;

        let session = entry.value_mut();
        if session.user_id != user_id {
            return Err(AppError::SessionNotFound);
        }
        if session.is_expired() {
            // Expiry revokes authority immediately, but the session remains as
            // durable cleanup state until its ephemeral locks and row have
            // both been deleted successfully by the expiry task.
            return Err(AppError::SessionExpired);
        }

        // Build the renewal without publishing it in memory. SQLite is the
        // durable authority across restarts, so persistence must succeed before
        // callers can observe or receive the extended expiry.
        let ttl = match ttl_seconds {
            Some(t) => clamp_ttl(Some(t)),
            None => session.ttl_seconds,
        };
        let new_expires = Utc::now() + chrono::Duration::seconds(ttl as i64);
        let mut updated = session.clone();
        updated.ttl_seconds = ttl;
        updated.expires_at = new_expires;

        self.save_session_to_database(&updated)?;
        *session = updated;
        drop(entry);

        debug!(
            "Session keepalive: {} new expiry {}",
            session_id, new_expires
        );
        Ok(new_expires)
    }

    #[cfg(test)]
    pub fn terminate_session(&self, session_id: Uuid, user_id: Uuid) -> Result<()> {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        self.terminate_session_locked(session_id, user_id)
    }

    fn terminate_session_locked(&self, session_id: Uuid, user_id: Uuid) -> Result<()> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(AppError::SessionNotFound)?;

        if session.user_id != user_id {
            return Err(AppError::SessionNotFound);
        }
        drop(session);

        self.delete_session_from_database(session_id)?;
        self.sessions.remove(&session_id);

        info!("Session terminated: {}", session_id);
        Ok(())
    }

    /// Runs an operation only while the requested session is active and owned
    /// by the caller. Teardown uses the same guard, so a successful operation
    /// cannot publish a new session-bound lock after cleanup has already run.
    pub(crate) fn with_active_session<T, F>(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        operation: F,
    ) -> Result<T>
    where
        F: FnOnce(&Session) -> Result<T>,
    {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        let session = self
            .get_session(session_id)
            .ok_or(AppError::SessionNotFound)?;
        if session.user_id != user_id {
            return Err(AppError::SessionNotFound);
        }
        if session.is_expired() {
            return Err(AppError::SessionExpired);
        }
        operation(&session)
    }

    pub fn terminate_session_and_release_locks(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        lock_store: &LockStore,
    ) -> Result<()> {
        self.terminate_session_and_release_locks_with_hook(session_id, user_id, lock_store, || {})
    }

    fn terminate_session_and_release_locks_with_hook<F>(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        lock_store: &LockStore,
        after_cleanup: F,
    ) -> Result<()>
    where
        F: FnOnce(),
    {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        let session = self
            .get_session(session_id)
            .ok_or(AppError::SessionNotFound)?;
        if session.user_id != user_id {
            return Err(AppError::SessionNotFound);
        }
        lock_store.release_locks_for_session_checked(session_id)?;
        after_cleanup();
        self.terminate_session_locked(session_id, user_id)
    }

    pub fn get_session(&self, session_id: Uuid) -> Option<Session> {
        self.sessions
            .get(&session_id)
            .map(|entry| entry.value().clone())
    }

    #[allow(dead_code)]
    pub fn get_user_sessions(&self, user_id: Uuid) -> Vec<Session> {
        self.sessions
            .iter()
            .filter_map(|entry| {
                let s = entry.value();
                if s.user_id == user_id && !s.is_expired() {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn start_expiry_task(self, lock_store: LockStore) {
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(5));
            info!("Started session expiry background task (5s interval)");

            loop {
                interval.tick().await;
                self.cleanup_expired_sessions(&lock_store);
            }
        });
    }

    /// Releases ephemeral locks whose sessions were already absent or expired
    /// when the process restarted. Non-ephemeral locks retain their independent
    /// lease TTL, matching runtime expiry and explicit termination semantics.
    pub fn reconcile_ephemeral_locks(&self, lock_store: &LockStore) {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        if let Err(error) = self.cleanup_expired_sessions_locked(lock_store) {
            // Startup remains fail-closed: failed deletions leave the lock and
            // session rows intact, and the periodic task retries from them.
            warn!(
                "Startup session reconciliation incomplete; retained durable retry state: {}",
                error
            );
        }
    }

    fn cleanup_expired_sessions(&self, lock_store: &LockStore) {
        let _lifecycle = self.lifecycle_guard.lock().unwrap();
        if let Err(error) = self.cleanup_expired_sessions_locked(lock_store) {
            warn!(
                "Expired session cleanup incomplete; retained durable retry state: {}",
                error
            );
        }
    }

    fn cleanup_expired_sessions_locked(&self, lock_store: &LockStore) -> Result<()> {
        let now = Utc::now();
        let mut cleanup_candidates = HashSet::new();

        for entry in self.sessions.iter() {
            if entry.value().expires_at <= now {
                cleanup_candidates.insert(*entry.key());
            }
        }

        // An ephemeral lock is itself durable retry state when its session row
        // is already absent. Rediscover these orphans on every startup and
        // periodic pass until checked deletion succeeds.
        for lock in lock_store.list_locks(None) {
            if let Some(session_id) = lock.session_id.filter(|_| lock.ephemeral) {
                if self
                    .get_session(session_id)
                    .is_none_or(|session| session.is_expired())
                {
                    cleanup_candidates.insert(session_id);
                }
            }
        }

        let mut first_error = None;
        for session_id in cleanup_candidates {
            if let Err(error) = lock_store.release_locks_for_session_checked(session_id) {
                warn!(
                    "Failed to release ephemeral locks for expired session {}: {}",
                    session_id, error
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }

            if let Err(error) = self.delete_session_from_database(session_id) {
                warn!(
                    "Failed to delete expired session {} from database: {}",
                    session_id, error
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }

            if let Some((_, session)) = self.sessions.remove(&session_id) {
                debug!("Session expired: {} (user {})", session_id, session.user_id);
            } else {
                debug!(
                    "Removed orphaned ephemeral locks for absent session {}",
                    session_id
                );
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn save_session_to_database(&self, session: &Session) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO sessions (id, user_id, ttl_seconds, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
            params![
                session.id.to_string(),
                session.user_id.to_string(),
                session.ttl_seconds,
                session.expires_at.to_rfc3339(),
                session.created_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    fn delete_session_from_database(&self, session_id: Uuid) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM sessions WHERE id = ?",
            params![session_id.to_string()],
        )?;
        Ok(())
    }
}

fn clamp_ttl(ttl: Option<u32>) -> u32 {
    ttl.unwrap_or(DEFAULT_TTL).clamp(MIN_TTL, MAX_TTL)
}

// ── Route handlers ──────────────────────────────────────────────────────

pub async fn create_session(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>)> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let session = state
        .session_store
        .create_session(user_id, req.ttl_seconds)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: session.id,
            expires_at: session.expires_at,
            keepalive_interval_secs: session.ttl_seconds / 2,
        }),
    ))
}

pub async fn keepalive(
    Path(id): Path<Uuid>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<KeepAliveResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let new_expires = state.session_store.keepalive(id, user_id, None)?;

    Ok(Json(KeepAliveResponse {
        session_id: id,
        expires_at: new_expires,
    }))
}

pub async fn terminate_session(
    Path(id): Path<Uuid>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    let user_id = state.auth_service.authenticate(&headers)?;

    state.session_store.terminate_session_and_release_locks(
        id,
        user_id,
        &state.lock_handlers.store,
    )?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_session_status(
    Path(id): Path<Uuid>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionStatusResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let session = state
        .session_store
        .get_session(id)
        .filter(|session| session.user_id == user_id)
        .ok_or(AppError::SessionNotFound)?;

    let lock_count = state.lock_handlers.store.count_session_locks(id);

    Ok(Json(SessionStatusResponse {
        session_id: session.id,
        user_id: session.user_id,
        expires_at: session.expires_at,
        lock_count,
        active: !session.is_expired(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AcquireLockOptions, LockStore};
    use rusqlite::Connection;
    use std::sync::{mpsc, Barrier};
    use std::thread;
    use tempfile::NamedTempFile;

    fn make_db(path: &str) -> DbConn {
        Arc::new(Mutex::new(
            Connection::open(path).expect("Failed to open DB"),
        ))
    }

    fn create_test_stores(db_path: &str) -> (SessionStore, LockStore) {
        let db = make_db(db_path);
        let lock_store = LockStore::new(db.clone(), 1).expect("Failed to create lock store");
        let session_store = SessionStore::new(db).expect("Failed to create session store");
        (session_store, lock_store)
    }

    fn expire_session(session_store: &SessionStore, session_id: Uuid) {
        let expired_at = Utc::now() - chrono::Duration::seconds(1);
        {
            let mut session = session_store.sessions.get_mut(&session_id).unwrap();
            session.expires_at = expired_at;
        }
        session_store
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET expires_at = ? WHERE id = ?",
                params![expired_at.to_rfc3339(), session_id.to_string()],
            )
            .unwrap();
    }

    fn install_lock_delete_failure(session_store: &SessionStore) {
        session_store
            .db
            .lock()
            .unwrap()
            .execute_batch(
                r#"
                CREATE TRIGGER fail_ephemeral_lock_delete
                BEFORE DELETE ON locks
                BEGIN
                    SELECT RAISE(ABORT, 'injected lock delete failure');
                END;
                "#,
            )
            .unwrap();
    }

    fn remove_lock_delete_failure(session_store: &SessionStore) {
        session_store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_ephemeral_lock_delete")
            .unwrap();
    }

    #[test]
    fn test_create_session() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        let session = store.create_session(user_id, Some(60)).unwrap();
        assert_eq!(session.user_id, user_id);
        assert_eq!(session.ttl_seconds, 60);
        assert!(!session.is_expired());

        let fetched = store.get_session(session.id).unwrap();
        assert_eq!(fetched.id, session.id);
    }

    #[test]
    fn test_create_session_default_ttl() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        let session = store.create_session(user_id, None).unwrap();
        assert_eq!(session.ttl_seconds, 60);
    }

    #[test]
    fn test_create_session_ttl_clamping() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        // Below min
        let s1 = store.create_session(user_id, Some(1)).unwrap();
        assert_eq!(s1.ttl_seconds, MIN_TTL);

        // Above max
        let s2 = store.create_session(user_id, Some(9999)).unwrap();
        assert_eq!(s2.ttl_seconds, MAX_TTL);
    }

    #[test]
    fn test_keepalive() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        let session = store.create_session(user_id, Some(30)).unwrap();
        let original_expires = session.expires_at;

        std::thread::sleep(std::time::Duration::from_millis(50));

        let new_expires = store.keepalive(session.id, user_id, None).unwrap();
        assert!(new_expires > original_expires);
    }

    #[test]
    fn test_keepalive_wrong_user() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();

        let session = store.create_session(user_id, Some(60)).unwrap();
        let result = store.keepalive(session.id, other_user, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_keepalive_nonexistent_session() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());

        let result = store.keepalive(Uuid::new_v4(), Uuid::new_v4(), None);
        assert!(result.is_err());
    }

    #[test]
    fn keepalive_write_failure_preserves_memory_and_restart_authority() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let user_id = Uuid::new_v4();
        let session_id;
        let original_expires;

        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            let session = session_store.create_session(user_id, Some(60)).unwrap();
            session_id = session.id;
            original_expires = Utc::now() + chrono::Duration::seconds(2);

            lock_store
                .acquire_lock(
                    "failed-keepalive-ephemeral".to_string(),
                    session_id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session_id))
                        .ephemeral(true),
                )
                .unwrap();

            {
                let mut entry = session_store.sessions.get_mut(&session_id).unwrap();
                entry.expires_at = original_expires;
            }
            {
                let db = session_store.db.lock().unwrap();
                db.execute(
                    "UPDATE sessions SET expires_at = ? WHERE id = ?",
                    params![original_expires.to_rfc3339(), session_id.to_string()],
                )
                .unwrap();
                db.execute_batch("PRAGMA query_only = ON").unwrap();
            }

            let error = session_store
                .keepalive(session_id, user_id, Some(300))
                .unwrap_err();
            assert!(matches!(error, AppError::Database(_)));

            let in_memory = session_store.get_session(session_id).unwrap();
            assert_eq!(in_memory.ttl_seconds, 60);
            assert_eq!(in_memory.expires_at, original_expires);
            let persisted_expires = session_store
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT expires_at FROM sessions WHERE id = ?",
                    params![session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            assert_eq!(persisted_expires, original_expires.to_rfc3339());
            assert!(lock_store.get_lock("failed-keepalive-ephemeral").is_some());
        }

        let until_expired = original_expires
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or_default();
        std::thread::sleep(until_expired + std::time::Duration::from_millis(50));

        let (session_store, lock_store) = create_test_stores(&db_path);
        assert!(session_store
            .get_session(session_id)
            .is_some_and(|session| session.is_expired()));
        assert!(lock_store.get_lock("failed-keepalive-ephemeral").is_some());
        session_store.reconcile_ephemeral_locks(&lock_store);
        assert!(session_store.get_session(session_id).is_none());
        assert!(lock_store.get_lock("failed-keepalive-ephemeral").is_none());
    }

    #[test]
    fn test_terminate_session() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        let session = store.create_session(user_id, Some(60)).unwrap();
        assert!(store.get_session(session.id).is_some());

        store.terminate_session(session.id, user_id).unwrap();
        assert!(store.get_session(session.id).is_none());
    }

    #[test]
    fn test_terminate_wrong_user() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();
        let other_user = Uuid::new_v4();

        let session = store.create_session(user_id, Some(60)).unwrap();
        let result = store.terminate_session(session.id, other_user);
        assert!(result.is_err());
        assert!(store.get_session(session.id).is_some());
    }

    #[test]
    fn teardown_waits_for_inflight_session_lock_then_removes_it() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let session_store = Arc::new(session_store);
        let lock_store = Arc::new(lock_store);
        let user_id = Uuid::new_v4();
        let session = session_store.create_session(user_id, Some(60)).unwrap();
        let validated = Arc::new(Barrier::new(2));
        let finish_acquire = Arc::new(Barrier::new(2));

        let acquiring_sessions = Arc::clone(&session_store);
        let acquiring_locks = Arc::clone(&lock_store);
        let acquiring_validated = Arc::clone(&validated);
        let acquiring_finish = Arc::clone(&finish_acquire);
        let acquisition = thread::spawn(move || {
            acquiring_sessions.with_active_session(session.id, user_id, |_| {
                acquiring_validated.wait();
                acquiring_finish.wait();
                acquiring_locks.acquire_lock_snapshot(
                    "teardown-race".to_string(),
                    session.id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session.id))
                        .ephemeral(true),
                )
            })
        });

        validated.wait();
        let (teardown_started_tx, teardown_started_rx) = mpsc::channel();
        let terminating_sessions = Arc::clone(&session_store);
        let terminating_locks = Arc::clone(&lock_store);
        let teardown = thread::spawn(move || {
            teardown_started_tx.send(()).unwrap();
            terminating_sessions.terminate_session_and_release_locks(
                session.id,
                user_id,
                &terminating_locks,
            )
        });
        teardown_started_rx.recv().unwrap();
        finish_acquire.wait();

        acquisition.join().unwrap().unwrap();
        teardown.join().unwrap().unwrap();
        assert!(session_store.get_session(session.id).is_none());
        assert!(lock_store.get_lock("teardown-race").is_none());
    }

    #[test]
    fn teardown_waits_for_inflight_session_renewal_then_removes_the_lock() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let session_store = Arc::new(session_store);
        let lock_store = Arc::new(lock_store);
        let user_id = Uuid::new_v4();
        let session = session_store.create_session(user_id, Some(60)).unwrap();
        let (lease_id, _, _) = lock_store
            .acquire_lock(
                "renew-teardown-race".to_string(),
                session.id,
                AcquireLockOptions::new(60)
                    .with_session_id(Some(session.id))
                    .ephemeral(true),
            )
            .unwrap();
        let validated = Arc::new(Barrier::new(2));
        let finish_renew = Arc::new(Barrier::new(2));

        let renewing_sessions = Arc::clone(&session_store);
        let renewing_locks = Arc::clone(&lock_store);
        let renewing_validated = Arc::clone(&validated);
        let renewing_finish = Arc::clone(&finish_renew);
        let renewal = thread::spawn(move || {
            renewing_sessions.with_active_session(session.id, user_id, |_| {
                renewing_validated.wait();
                renewing_finish.wait();
                renewing_locks.renew_lock("renew-teardown-race", lease_id, session.id, 300)
            })
        });

        validated.wait();
        let (teardown_started_tx, teardown_started_rx) = mpsc::channel();
        let terminating_sessions = Arc::clone(&session_store);
        let terminating_locks = Arc::clone(&lock_store);
        let teardown = thread::spawn(move || {
            teardown_started_tx.send(()).unwrap();
            terminating_sessions.terminate_session_and_release_locks(
                session.id,
                user_id,
                &terminating_locks,
            )
        });
        teardown_started_rx.recv().unwrap();
        finish_renew.wait();

        renewal.join().unwrap().unwrap();
        teardown.join().unwrap().unwrap();
        assert!(session_store.get_session(session.id).is_none());
        assert!(lock_store.get_lock("renew-teardown-race").is_none());
    }

    #[test]
    fn acquire_after_teardown_cleanup_cannot_publish_an_orphan_lock() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let session_store = Arc::new(session_store);
        let lock_store = Arc::new(lock_store);
        let user_id = Uuid::new_v4();
        let session = session_store.create_session(user_id, Some(60)).unwrap();
        let cleanup_reached = Arc::new(Barrier::new(2));
        let finish_teardown = Arc::new(Barrier::new(2));

        let terminating_sessions = Arc::clone(&session_store);
        let terminating_locks = Arc::clone(&lock_store);
        let terminating_cleanup = Arc::clone(&cleanup_reached);
        let terminating_finish = Arc::clone(&finish_teardown);
        let teardown = thread::spawn(move || {
            terminating_sessions.terminate_session_and_release_locks_with_hook(
                session.id,
                user_id,
                &terminating_locks,
                || {
                    terminating_cleanup.wait();
                    terminating_finish.wait();
                },
            )
        });

        cleanup_reached.wait();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let acquiring_sessions = Arc::clone(&session_store);
        let acquiring_locks = Arc::clone(&lock_store);
        let acquisition = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            acquiring_sessions.with_active_session(session.id, user_id, |_| {
                acquiring_locks.acquire_lock_snapshot(
                    "post-cleanup-race".to_string(),
                    session.id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session.id))
                        .ephemeral(true),
                )
            })
        });
        attempted_rx.recv().unwrap();
        finish_teardown.wait();

        teardown.join().unwrap().unwrap();
        assert!(matches!(
            acquisition.join().unwrap(),
            Err(AppError::SessionNotFound)
        ));
        assert!(lock_store.get_lock("post-cleanup-race").is_none());
    }

    #[test]
    fn explicit_teardown_does_not_acknowledge_failed_lock_cleanup() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();
        let session = session_store.create_session(user_id, Some(60)).unwrap();
        lock_store
            .acquire_lock(
                "failed-explicit-cleanup".to_string(),
                session.id,
                AcquireLockOptions::new(300)
                    .with_session_id(Some(session.id))
                    .ephemeral(true),
            )
            .unwrap();
        session_store
            .db
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only = ON")
            .unwrap();

        assert!(matches!(
            session_store.terminate_session_and_release_locks(session.id, user_id, &lock_store,),
            Err(AppError::Database(_))
        ));
        assert!(session_store.get_session(session.id).is_some());
        assert!(lock_store.get_lock("failed-explicit-cleanup").is_some());
    }

    #[test]
    fn test_get_user_sessions() {
        let tmp = NamedTempFile::new().unwrap();
        let (store, _lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user1 = Uuid::new_v4();
        let user2 = Uuid::new_v4();

        store.create_session(user1, Some(60)).unwrap();
        store.create_session(user1, Some(60)).unwrap();
        store.create_session(user2, Some(60)).unwrap();

        assert_eq!(store.get_user_sessions(user1).len(), 2);
        assert_eq!(store.get_user_sessions(user2).len(), 1);
    }

    #[tokio::test]
    async fn test_session_expiry_releases_locks() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        // Create a session with very short TTL
        let session = session_store
            .create_session(user_id, Some(MIN_TTL))
            .unwrap();

        // Acquire a lock with this session
        let (_lease_id, _, _) = lock_store
            .acquire_lock(
                "session-lock".to_string(),
                session.id,
                AcquireLockOptions::new(300)
                    .with_session_id(Some(session.id))
                    .ephemeral(true),
            )
            .unwrap();

        // Verify lock exists
        assert!(lock_store.get_lock("session-lock").is_some());

        // Manually expire the session and run cleanup
        expire_session(&session_store, session.id);

        session_store.cleanup_expired_sessions(&lock_store);

        // Session should be gone
        assert!(session_store.get_session(session.id).is_none());
        // Lock should be released
        assert!(lock_store.get_lock("session-lock").is_none());
    }

    #[test]
    fn session_expiry_releases_only_ephemeral_locks() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();
        let session = session_store.create_session(user_id, Some(60)).unwrap();

        for (name, ephemeral) in [("ephemeral", true), ("durable", false)] {
            lock_store
                .acquire_lock(
                    name.to_string(),
                    if ephemeral { session.id } else { user_id },
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session.id))
                        .ephemeral(ephemeral),
                )
                .unwrap();
        }

        lock_store
            .release_locks_for_session_checked(session.id)
            .unwrap();
        assert!(lock_store.get_lock("ephemeral").is_none());
        assert!(lock_store.get_lock("durable").is_some());
    }

    #[test]
    fn runtime_expiry_and_keepalive_retain_retry_state_after_lock_delete_failure() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let user_id = Uuid::new_v4();

        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            let session = session_store.create_session(user_id, Some(60)).unwrap();
            lock_store
                .acquire_lock(
                    "runtime-cleanup-retry".to_string(),
                    session.id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session.id))
                        .ephemeral(true),
                )
                .unwrap();
            expire_session(&session_store, session.id);
            install_lock_delete_failure(&session_store);

            assert!(matches!(
                session_store.keepalive(session.id, user_id, None),
                Err(AppError::SessionExpired)
            ));
            assert!(session_store.get_session(session.id).is_some());

            session_store.cleanup_expired_sessions(&lock_store);
            assert!(session_store
                .get_session(session.id)
                .is_some_and(|retained| retained.is_expired()));
            assert!(lock_store.get_lock("runtime-cleanup-retry").is_some());
            let db = session_store.db.lock().unwrap();
            let session_rows: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id = ?",
                    params![session.id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let lock_rows: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM locks WHERE name = 'runtime-cleanup-retry'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(session_rows, 1);
            assert_eq!(lock_rows, 1);
            drop(db);

            remove_lock_delete_failure(&session_store);
            session_store.cleanup_expired_sessions(&lock_store);
            assert!(session_store.get_session(session.id).is_none());
            assert!(lock_store.get_lock("runtime-cleanup-retry").is_none());
        }

        let (session_store, lock_store) = create_test_stores(&db_path);
        assert!(session_store.get_user_sessions(user_id).is_empty());
        assert!(lock_store.get_lock("runtime-cleanup-retry").is_none());
    }

    #[test]
    fn runtime_expiry_retries_until_session_row_deletion_succeeds() {
        let tmp = NamedTempFile::new().unwrap();
        let (session_store, lock_store) = create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();
        let session = session_store.create_session(user_id, Some(60)).unwrap();
        lock_store
            .acquire_lock(
                "session-row-cleanup-retry".to_string(),
                session.id,
                AcquireLockOptions::new(300)
                    .with_session_id(Some(session.id))
                    .ephemeral(true),
            )
            .unwrap();
        expire_session(&session_store, session.id);
        session_store
            .db
            .lock()
            .unwrap()
            .execute_batch(
                r#"
                CREATE TRIGGER fail_expired_session_delete
                BEFORE DELETE ON sessions
                BEGIN
                    SELECT RAISE(ABORT, 'injected session delete failure');
                END;
                "#,
            )
            .unwrap();

        session_store.cleanup_expired_sessions(&lock_store);
        assert!(lock_store.get_lock("session-row-cleanup-retry").is_none());
        assert!(session_store
            .get_session(session.id)
            .is_some_and(|retained| retained.is_expired()));

        session_store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_expired_session_delete")
            .unwrap();
        session_store.cleanup_expired_sessions(&lock_store);
        assert!(session_store.get_session(session.id).is_none());
    }

    #[test]
    fn restart_reconciles_expired_session_ephemeral_locks() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let session_id;
        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            let session = session_store
                .create_session(Uuid::new_v4(), Some(60))
                .unwrap();
            session_id = session.id;
            lock_store
                .acquire_lock(
                    "restart-ephemeral".to_string(),
                    session.id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session.id))
                        .ephemeral(true),
                )
                .unwrap();
            let db = session_store.db.lock().unwrap();
            db.execute(
                "UPDATE sessions SET expires_at = ? WHERE id = ?",
                params![
                    (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(),
                    session.id.to_string()
                ],
            )
            .unwrap();
        }

        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            assert!(session_store
                .get_session(session_id)
                .is_some_and(|session| session.is_expired()));
            assert!(lock_store.get_lock("restart-ephemeral").is_some());
            session_store.reconcile_ephemeral_locks(&lock_store);
            assert!(session_store.get_session(session_id).is_none());
            assert!(lock_store.get_lock("restart-ephemeral").is_none());
        }

        let (_session_store, lock_store) = create_test_stores(&db_path);
        assert!(lock_store.get_lock("restart-ephemeral").is_none());
    }

    #[test]
    fn startup_reconciliation_retries_failed_lock_delete_across_restart() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let session_id;

        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            let session = session_store
                .create_session(Uuid::new_v4(), Some(60))
                .unwrap();
            session_id = session.id;
            lock_store
                .acquire_lock(
                    "restart-cleanup-retry".to_string(),
                    session.id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session.id))
                        .ephemeral(true),
                )
                .unwrap();
            expire_session(&session_store, session.id);
            install_lock_delete_failure(&session_store);
        }

        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            session_store.reconcile_ephemeral_locks(&lock_store);
            assert!(session_store
                .get_session(session_id)
                .is_some_and(|session| session.is_expired()));
            assert!(lock_store.get_lock("restart-cleanup-retry").is_some());
        }

        Connection::open(&db_path)
            .unwrap()
            .execute_batch("DROP TRIGGER fail_ephemeral_lock_delete")
            .unwrap();

        {
            let (session_store, lock_store) = create_test_stores(&db_path);
            assert!(session_store
                .get_session(session_id)
                .is_some_and(|session| session.is_expired()));
            assert!(lock_store.get_lock("restart-cleanup-retry").is_some());
            session_store.reconcile_ephemeral_locks(&lock_store);
            assert!(session_store.get_session(session_id).is_none());
            assert!(lock_store.get_lock("restart-cleanup-retry").is_none());
        }

        let (session_store, lock_store) = create_test_stores(&db_path);
        assert!(session_store.get_session(session_id).is_none());
        assert!(lock_store.get_lock("restart-cleanup-retry").is_none());
    }

    #[test]
    fn test_session_persistence() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let user_id = Uuid::new_v4();
        let session_id;

        // Create session in first store
        {
            let (store, _lock_store) = create_test_stores(&db_path);
            let session = store.create_session(user_id, Some(120)).unwrap();
            session_id = session.id;
        }

        // Load in second store — session should survive
        {
            let (store, _lock_store) = create_test_stores(&db_path);
            let loaded = store.get_session(session_id);
            assert!(loaded.is_some());
            assert_eq!(loaded.unwrap().user_id, user_id);
        }
    }
}
