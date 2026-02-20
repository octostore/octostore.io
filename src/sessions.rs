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
use std::sync::Arc;
use std::time::Duration;
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
        };

        store.load_sessions_from_database()?;

        Ok(store)
    }

    fn load_sessions_from_database(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let now = Utc::now();

        let mut stmt = db.prepare(
            "SELECT id, user_id, ttl_seconds, expires_at, created_at FROM sessions",
        )?;

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
        let mut expired_ids = Vec::new();

        for row in rows {
            let session = row?;
            if session.expires_at > now {
                self.sessions.insert(session.id, session);
                loaded += 1;
            } else {
                expired_ids.push(session.id);
            }
        }

        if !expired_ids.is_empty() {
            for id in &expired_ids {
                if let Err(e) = db.execute(
                    "DELETE FROM sessions WHERE id = ?",
                    params![id.to_string()],
                ) {
                    warn!("Failed to delete expired session {} from database: {}", id, e);
                }
            }
        }

        info!(
            "Loaded {} active sessions, cleaned up {} expired",
            loaded,
            expired_ids.len()
        );

        Ok(())
    }

    pub fn create_session(&self, user_id: Uuid, ttl_seconds: Option<u32>) -> Result<Session> {
        let ttl = clamp_ttl(ttl_seconds);
        let now = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            user_id,
            ttl_seconds: ttl,
            expires_at: now + chrono::Duration::seconds(ttl as i64),
            created_at: now,
        };

        self.sessions.insert(session.id, session.clone());
        self.save_session_to_database(&session)?;

        info!("Session created: {} for user {}", session.id, user_id);
        Ok(session)
    }

    pub fn keepalive(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        ttl_seconds: Option<u32>,
    ) -> Result<DateTime<Utc>> {
        let mut entry = self
            .sessions
            .get_mut(&session_id)
            .ok_or(AppError::SessionNotFound)?;

        let session = entry.value_mut();
        if session.user_id != user_id {
            return Err(AppError::SessionNotFound);
        }
        if session.is_expired() {
            drop(entry);
            self.sessions.remove(&session_id);
            return Err(AppError::SessionExpired);
        }

        // Extend by the original TTL, or a new one if provided
        let ttl = match ttl_seconds {
            Some(t) => {
                let clamped = clamp_ttl(Some(t));
                session.ttl_seconds = clamped;
                clamped
            }
            None => session.ttl_seconds,
        };
        let new_expires = Utc::now() + chrono::Duration::seconds(ttl as i64);
        session.expires_at = new_expires;

        let updated = session.clone();
        drop(entry);
        if let Err(e) = self.save_session_to_database(&updated) {
            warn!("Failed to update session {} in database: {}", session_id, e);
        }

        debug!("Session keepalive: {} new expiry {}", session_id, new_expires);
        Ok(new_expires)
    }

    pub fn terminate_session(&self, session_id: Uuid, user_id: Uuid) -> Result<()> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(AppError::SessionNotFound)?;

        if session.user_id != user_id {
            return Err(AppError::SessionNotFound);
        }
        drop(session);

        self.sessions.remove(&session_id);
        self.delete_session_from_database(session_id)?;

        info!("Session terminated: {}", session_id);
        Ok(())
    }

    pub fn get_session(&self, session_id: Uuid) -> Option<Session> {
        self.sessions
            .get(&session_id)
            .map(|entry| entry.value().clone())
    }

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

    fn cleanup_expired_sessions(&self, lock_store: &LockStore) {
        let now = Utc::now();
        let mut expired = Vec::new();

        for entry in self.sessions.iter() {
            if entry.value().expires_at <= now {
                expired.push(entry.key().clone());
            }
        }

        for session_id in expired {
            if let Some((_, session)) = self.sessions.remove(&session_id) {
                debug!(
                    "Session expired: {} (user {})",
                    session_id, session.user_id
                );
                lock_store.release_locks_for_session(session_id);
                if let Err(e) = self.delete_session_from_database(session_id) {
                    warn!(
                        "Failed to delete expired session {} from database: {}",
                        session_id, e
                    );
                }
            }
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
    ttl.unwrap_or(DEFAULT_TTL).max(MIN_TTL).min(MAX_TTL)
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

    // Release all locks tied to this session before terminating
    state
        .lock_handlers
        .store
        .release_locks_for_session(id);

    state.session_store.terminate_session(id, user_id)?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_session_status(
    Path(id): Path<Uuid>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionStatusResponse>> {
    let _user_id = state.auth_service.authenticate(&headers)?;

    let session = state
        .session_store
        .get_session(id)
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
    use crate::store::LockStore;
    use rusqlite::Connection;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    fn make_db(path: &str) -> DbConn {
        Arc::new(Mutex::new(
            Connection::open(path).expect("Failed to open DB"),
        ))
    }

    fn create_test_stores(
        db_path: &str,
    ) -> (SessionStore, LockStore) {
        let db = make_db(db_path);
        let lock_store = LockStore::new(db.clone(), 1).expect("Failed to create lock store");
        let session_store = SessionStore::new(db).expect("Failed to create session store");
        (session_store, lock_store)
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
        let (session_store, lock_store) =
            create_test_stores(tmp.path().to_str().unwrap());
        let user_id = Uuid::new_v4();

        // Create a session with very short TTL
        let session = session_store.create_session(user_id, Some(MIN_TTL)).unwrap();

        // Acquire a lock with this session
        let (lease_id, _, _) = lock_store
            .acquire_lock(
                "session-lock".to_string(),
                user_id,
                300,
                None,
                Some(session.id),
            )
            .unwrap();

        // Verify lock exists
        assert!(lock_store.get_lock("session-lock").is_some());

        // Manually expire the session and run cleanup
        {
            let mut entry = session_store.sessions.get_mut(&session.id).unwrap();
            entry.expires_at = Utc::now() - chrono::Duration::seconds(1);
        }

        session_store.cleanup_expired_sessions(&lock_store);

        // Session should be gone
        assert!(session_store.get_session(session.id).is_none());
        // Lock should be released
        assert!(lock_store.get_lock("session-lock").is_none());
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
