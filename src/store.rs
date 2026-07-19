use crate::{
    error::Result,
    models::{Lock, LockEvent, LockEventType},
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::sync::broadcast;
use tokio::time;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// A shared, thread-safe SQLite connection used by both [`LockStore`] and
/// [`crate::auth::AuthService`] so the process opens only one file handle.
pub type DbConn = Arc<Mutex<Connection>>;

pub fn configure_sqlite_connection(db: &DbConn) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(())
}

/// In-memory lock store backed by SQLite for durability.
///
/// Locks live in a `DashMap` for fast concurrent access. Every mutation is
/// also written to SQLite so locks survive process restarts. On startup the
/// store replays unexpired rows from the database.
#[derive(Clone)]
pub struct LockStore {
    locks: Arc<DashMap<String, Lock>>,
    fencing_counter: Arc<AtomicU64>,
    db: DbConn,
    /// Serializes admission checks that depend on an active-lock count.
    admission_guard: Arc<Mutex<()>>,
    /// Registry of broadcast channels for lock status watchers.
    watch_channels: Arc<DashMap<String, broadcast::Sender<LockEvent>>>,
    /// Tracks locks in their grace/cooldown period after release or expiry.
    cooling_locks: Arc<DashMap<String, (DateTime<Utc>, u32)>>,
    /// Webhook store for dispatching lock events (set after construction).
    webhook_store: Arc<OnceLock<crate::webhooks::WebhookStore>>,
}

#[derive(Debug, Clone, Default)]
pub struct AcquireLockOptions {
    pub ttl_seconds: u32,
    pub metadata: Option<String>,
    pub session_id: Option<Uuid>,
    pub ephemeral: bool,
    pub lock_delay_seconds: u32,
}

#[derive(Debug, Clone)]
pub enum AcquireLockOutcome {
    Acquired(Lock),
    Held(Lock),
}

impl AcquireLockOptions {
    pub fn new(ttl_seconds: u32) -> Self {
        Self {
            ttl_seconds,
            ..Self::default()
        }
    }

    pub fn with_metadata(mut self, metadata: Option<String>) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_session_id(mut self, session_id: Option<Uuid>) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    pub fn with_lock_delay_seconds(mut self, lock_delay_seconds: u32) -> Self {
        self.lock_delay_seconds = lock_delay_seconds;
        self
    }
}

impl LockStore {
    pub fn new(db: DbConn, initial_fencing_token: u64) -> Result<Self> {
        configure_sqlite_connection(&db)?;

        // Create the lock and fencing tables if they do not exist. LockStore
        // owns fencing-token durability because every successful acquisition
        // must reserve its token before it is returned to a client.
        let persisted_fencing_token;
        {
            let conn = db.lock().unwrap();
            conn.execute(
                r#"
            CREATE TABLE IF NOT EXISTS locks (
                name TEXT PRIMARY KEY,
                holder_id TEXT NOT NULL,
                lease_id TEXT NOT NULL,
                fencing_token INTEGER NOT NULL,
                expires_at TEXT NOT NULL,
                metadata TEXT,
                acquired_at TEXT NOT NULL,
                session_id TEXT,
                ephemeral INTEGER NOT NULL DEFAULT 0,
                lock_delay_seconds INTEGER NOT NULL DEFAULT 0
            )
            "#,
                [],
            )?;
            // Migration: add session_id column if missing (existing DBs)
            let has_session_id = conn
                .prepare("PRAGMA table_info(locks)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .any(|name| name == "session_id");
            if !has_session_id {
                conn.execute("ALTER TABLE locks ADD COLUMN session_id TEXT", [])?;
            }
            let columns: Vec<String> = conn
                .prepare("PRAGMA table_info(locks)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            if !columns.iter().any(|n| n == "ephemeral") {
                conn.execute(
                    "ALTER TABLE locks ADD COLUMN ephemeral INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            if !columns.iter().any(|n| n == "lock_delay_seconds") {
                conn.execute(
                    "ALTER TABLE locks ADD COLUMN lock_delay_seconds INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }

            conn.execute(
                r#"
CREATE TABLE IF NOT EXISTS lock_acls (
    name TEXT PRIMARY KEY,
    acquire_acl TEXT NOT NULL
)
"#,
                [],
            )?;
            conn.execute(
                r#"
CREATE TABLE IF NOT EXISTS fencing_counter (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    counter INTEGER NOT NULL DEFAULT 1
                )
                "#,
                [],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO fencing_counter (id, counter) VALUES (1, 1)",
                [],
            )?;
            persisted_fencing_token = conn.query_row(
                "SELECT counter FROM fencing_counter WHERE id = 1",
                [],
                |row| row.get::<_, u64>(0),
            )?;
        }

        info!("Locks table initialized");

        // Create the store instance
        let store = Self {
            locks: Arc::new(DashMap::new()),
            fencing_counter: Arc::new(AtomicU64::new(
                initial_fencing_token.max(persisted_fencing_token).max(1),
            )),
            db,
            admission_guard: Arc::new(Mutex::new(())),
            watch_channels: Arc::new(DashMap::new()),
            cooling_locks: Arc::new(DashMap::new()),
            webhook_store: Arc::new(OnceLock::new()),
        };

        // Load existing unexpired locks from database
        store.load_locks_from_database()?;

        // Update fencing counter based on loaded locks
        store.update_fencing_counter_from_locks()?;

        Ok(store)
    }

    fn load_locks_from_database(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let now = Utc::now();

        let mut stmt = db.prepare(
            "SELECT name, holder_id, lease_id, fencing_token, expires_at, metadata, acquired_at, session_id, ephemeral, lock_delay_seconds FROM locks"
        )?;

        let lock_rows = stmt.query_map([], |row| {
            let holder_id_str: String = row.get(1)?;
            let lease_id_str: String = row.get(2)?;
            let expires_at_str: String = row.get(4)?;
            let acquired_at_str: String = row.get(6)?;
            let session_id_str: Option<String> = row.get(7)?;

            // Map parse errors to rusqlite's InvalidColumnType for propagation
            let holder_id = Uuid::parse_str(&holder_id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let lease_id = Uuid::parse_str(&lease_id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let expires_at = DateTime::parse_from_rfc3339(&expires_at_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&chrono::Utc);
            let acquired_at = DateTime::parse_from_rfc3339(&acquired_at_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&chrono::Utc);
            let session_id = session_id_str.and_then(|s| Uuid::parse_str(&s).ok());
            let ephemeral: bool = row.get::<_, i64>(8).unwrap_or(0) != 0;
            let lock_delay_seconds: u32 = row.get::<_, i64>(9).unwrap_or(0) as u32;

            Ok(Lock {
                name: row.get(0)?,
                holder_id,
                lease_id,
                fencing_token: row.get::<_, i64>(3)? as u64,
                expires_at,
                metadata: row.get(5)?,
                acquired_at,
                session_id,
                ephemeral,
                lock_delay_seconds,
            })
        })?;

        let mut loaded_count = 0;
        let mut expired_count = 0;
        let mut expired_names = Vec::new();

        for lock_result in lock_rows {
            let lock = lock_result?;

            if lock.expires_at > now {
                // Lock is still valid, load it into memory
                self.locks.insert(lock.name.clone(), lock);
                loaded_count += 1;
            } else {
                // Lock has expired, mark for deletion
                expired_names.push(lock.name.clone());
                expired_count += 1;
            }
        }

        // Clean up expired locks from database
        if !expired_names.is_empty() {
            for name in expired_names {
                if let Err(e) = db.execute("DELETE FROM locks WHERE name = ?", params![name]) {
                    warn!(
                        "Failed to delete expired lock {} from database: {}",
                        name, e
                    );
                }
            }
        }

        info!(
            "Loaded {} active locks from database, cleaned up {} expired locks",
            loaded_count, expired_count
        );

        Ok(())
    }

    fn update_fencing_counter_from_locks(&self) -> Result<()> {
        let mut max_fencing_token = 0u64;

        for entry in self.locks.iter() {
            let lock = entry.value();
            if lock.fencing_token > max_fencing_token {
                max_fencing_token = lock.fencing_token;
            }
        }

        // Set fencing counter to max + 1, but don't go lower than current value
        let new_counter = std::cmp::max(
            max_fencing_token + 1,
            self.fencing_counter.load(Ordering::SeqCst),
        );
        self.fencing_counter.store(new_counter, Ordering::SeqCst);
        self.persist_fencing_counter(new_counter)?;

        info!(
            "Updated fencing counter to {} based on existing locks",
            new_counter
        );
        Ok(())
    }

    fn next_fencing_token(&self) -> Result<u64> {
        let token = self
            .fencing_counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                crate::error::AppError::Internal(anyhow::anyhow!("fencing token space exhausted"))
            })?;

        // MAX prevents a slower concurrent acquisition from overwriting a
        // newer persisted value. The stored counter is always the next term.
        self.persist_fencing_counter(token + 1)?;
        Ok(token)
    }

    fn persist_fencing_counter(&self, counter: u64) -> Result<()> {
        let counter = i64::try_from(counter).map_err(|_| {
            crate::error::AppError::Internal(anyhow::anyhow!(
                "fencing token exceeds SQLite integer range"
            ))
        })?;
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE fencing_counter SET counter = MAX(counter, ?) WHERE id = 1",
            params![counter],
        )?;
        Ok(())
    }

    pub fn start_expiry_task(self) {
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(5));
            info!("Started lock expiry background task (5s interval)");

            loop {
                interval.tick().await;
                self.cleanup_expired_locks().await;
            }
        });
    }

    async fn cleanup_expired_locks(&self) {
        let now = Utc::now();

        // Evict expired cooling entries
        let expired_cooling: Vec<String> = self
            .cooling_locks
            .iter()
            .filter(|entry| entry.value().0 <= now)
            .map(|entry| entry.key().clone())
            .collect();
        for name in expired_cooling {
            self.cooling_locks.remove(&name);
        }

        let mut expired_locks = Vec::new();

        for entry in self.locks.iter() {
            if entry.value().expires_at <= now {
                expired_locks.push(entry.key().clone());
            }
        }

        for lock_name in expired_locks {
            self.cleanup_expired_lock(&lock_name, &now);
        }
    }

    fn cleanup_expired_lock(&self, lock_name: &str, now: &DateTime<Utc>) {
        let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.locks.entry(lock_name.to_string())
        else {
            return;
        };

        // The name may have been reacquired after the expiry scan. Re-check
        // under the per-entry write lock and keep that lock until SQLite has
        // deleted the same lease, so cleanup cannot delete a newer row.
        if entry.get().expires_at > *now {
            return;
        }
        if let Err(error) = self.delete_lock_from_database(lock_name) {
            warn!(
                "Failed to delete expired lock {} from database: {}",
                lock_name, error
            );
            return;
        }

        let lock = entry.remove();
        if lock.lock_delay_seconds > 0 {
            let available_at = *now + chrono::Duration::seconds(lock.lock_delay_seconds as i64);
            self.cooling_locks.insert(
                lock_name.to_string(),
                (available_at, lock.lock_delay_seconds),
            );
        }

        debug!("Expired lock: {} (holder: {})", lock_name, lock.holder_id);
        self.broadcast_event(LockEvent {
            event: LockEventType::Expired,
            lock_name: lock_name.to_string(),
            lock: None,
            timestamp: *now,
        });
    }

    /// Sets the webhook store for dispatching lock events to registered webhooks.
    pub fn set_webhook_store(&self, ws: crate::webhooks::WebhookStore) {
        let _ = self.webhook_store.set(ws);
    }

    /// Broadcasts a lock event to all active watchers and dispatches to webhooks.
    fn broadcast_event(&self, event: LockEvent) {
        // Fire-and-forget webhook dispatch (before send consumes the event)
        if let Some(ws) = self.webhook_store.get() {
            ws.dispatch(&event);
        }
        if let Some(sender) = self.watch_channels.get(&event.lock_name) {
            // We ignore send errors as they just mean no receivers are currently active
            let _ = sender.send(event);
        }
    }

    /// Returns a broadcast receiver for real-time events on a specific lock.
    pub fn watch_lock(&self, name: &str) -> broadcast::Receiver<LockEvent> {
        let sender = self
            .watch_channels
            .entry(name.to_string())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(100);
                tx
            });
        sender.subscribe()
    }

    pub fn acquire_lock(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
    ) -> Result<(Uuid, u64, DateTime<Utc>)> {
        match self.acquire_lock_outcome(name, holder_id, options)? {
            AcquireLockOutcome::Acquired(lock) => {
                Ok((lock.lease_id, lock.fencing_token, lock.expires_at))
            }
            AcquireLockOutcome::Held(_) => Err(crate::error::AppError::LockHeld),
        }
    }

    fn acquire_lock_outcome(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
    ) -> Result<AcquireLockOutcome> {
        let AcquireLockOptions {
            ttl_seconds,
            metadata,
            session_id,
            ephemeral,
            lock_delay_seconds,
        } = options;

        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        // Try to insert if not present, or update if held by same user
        match self.locks.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lease_id = Uuid::new_v4();
                let fencing_token = self.next_fencing_token()?;
                let lock = Lock {
                    name: name.clone(),
                    holder_id,
                    lease_id,
                    fencing_token,
                    expires_at,
                    metadata: metadata.clone(),
                    acquired_at: now,
                    session_id,
                    ephemeral,
                    lock_delay_seconds,
                };
                self.save_lock_to_database(&lock)?;
                entry.insert(lock.clone());

                // Broadcast acquisition
                self.broadcast_event(LockEvent {
                    event: LockEventType::Acquired,
                    lock_name: name.clone(),
                    lock: Some(lock.clone()),
                    timestamp: now,
                });

                Ok(AcquireLockOutcome::Acquired(lock))
            }
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let existing_lock = entry.get();

                // If expired, replace it
                if existing_lock.is_expired() {
                    let lease_id = Uuid::new_v4();
                    let fencing_token = self.next_fencing_token()?;
                    let lock = Lock {
                        name: name.clone(),
                        holder_id,
                        lease_id,
                        fencing_token,
                        expires_at,
                        metadata: metadata.clone(),
                        acquired_at: now,
                        session_id,
                        ephemeral,
                        lock_delay_seconds,
                    };
                    self.save_lock_to_database(&lock)?;
                    entry.insert(lock.clone());

                    // Broadcast new acquisition
                    self.broadcast_event(LockEvent {
                        event: LockEventType::Acquired,
                        lock_name: name.clone(),
                        lock: Some(lock.clone()),
                        timestamp: now,
                    });

                    Ok(AcquireLockOutcome::Acquired(lock))
                }
                // If held by same user, return existing lease info (idempotent)
                else if existing_lock.holder_id == holder_id {
                    Ok(AcquireLockOutcome::Acquired(existing_lock.clone()))
                }
                // Otherwise, lock is held by someone else
                else {
                    Ok(AcquireLockOutcome::Held(existing_lock.clone()))
                }
            }
        }
    }

    /// Acquires a lock while enforcing a strict active-lock limit for a
    /// namespace. The count and acquisition share one admission guard, so
    /// concurrent first campaigns cannot both pass the limit check.
    pub fn acquire_lock_with_prefix_limit(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
        prefix: &str,
        max_active: usize,
    ) -> Result<AcquireLockOutcome> {
        debug_assert!(name.starts_with(prefix));
        let _admission = self.admission_guard.lock().unwrap();
        let target_is_active = self.locks.get(&name).is_some_and(|lock| !lock.is_expired());

        if !target_is_active {
            let active = self
                .locks
                .iter()
                .filter(|entry| entry.key().starts_with(prefix) && !entry.value().is_expired())
                .count();
            if active >= max_active {
                return Err(crate::error::AppError::Conflict(
                    "Public election capacity reached; retry later or self-host OctoStore"
                        .to_string(),
                ));
            }
        }

        self.acquire_lock_outcome(name, holder_id, options)
    }

    pub fn release_lock(&self, name: &str, lease_id: Uuid, holder_id: Uuid) -> Result<()> {
        match self.locks.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry)
                if entry.get().holder_id == holder_id && entry.get().lease_id == lease_id =>
            {
                let lock_delay = entry.get().lock_delay_seconds;

                // Durability is part of the API contract. Do not remove the
                // in-memory lease or report success until SQLite accepted it.
                self.delete_lock_from_database(name)?;
                entry.remove();

                // Start cooling period if lock has a delay
                if lock_delay > 0 {
                    let available_at = Utc::now() + chrono::Duration::seconds(lock_delay as i64);
                    self.cooling_locks
                        .insert(name.to_string(), (available_at, lock_delay));
                }

                // Broadcast release
                self.broadcast_event(LockEvent {
                    event: LockEventType::Released,
                    lock_name: name.to_string(),
                    lock: None,
                    timestamp: Utc::now(),
                });

                Ok(())
            }
            dashmap::mapref::entry::Entry::Occupied(_) => {
                Err(crate::error::AppError::InvalidLeaseId)
            }
            dashmap::mapref::entry::Entry::Vacant(_) => Err(crate::error::AppError::LockNotFound {
                name: name.to_string(),
            }),
        }
    }

    pub fn renew_lock(
        &self,
        name: &str,
        lease_id: Uuid,
        holder_id: Uuid,
        ttl_seconds: u32,
    ) -> Result<Lock> {
        match self.locks.get_mut(name) {
            Some(mut lock) if lock.holder_id == holder_id && lock.lease_id == lease_id => {
                let now = Utc::now();
                if lock.expires_at <= now {
                    return Err(crate::error::AppError::LockNotFound {
                        name: name.to_string(),
                    });
                }
                let new_expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
                let mut renewed_lock = lock.clone();
                renewed_lock.expires_at = new_expires_at;

                // Persist first so a successful response can never describe a
                // renewal that would disappear after a process restart.
                self.save_lock_to_database(&renewed_lock)?;
                *lock = renewed_lock;

                // Broadcast renewal
                self.broadcast_event(LockEvent {
                    event: LockEventType::Renewed,
                    lock_name: name.to_string(),
                    lock: Some(lock.clone()),
                    timestamp: now,
                });

                Ok(lock.clone())
            }
            Some(_) => Err(crate::error::AppError::InvalidLeaseId),
            None => Err(crate::error::AppError::LockNotFound {
                name: name.to_string(),
            }),
        }
    }

    pub fn get_lock(&self, name: &str) -> Option<Lock> {
        self.locks.get(name).map(|entry| entry.value().clone())
    }

    pub fn get_lock_acl(&self, name: &str) -> Result<Option<crate::models::LockAcl>> {
        let conn = self.db.lock().unwrap();
        let acl_json: Option<String> = conn
            .query_row(
                "SELECT acquire_acl FROM lock_acls WHERE name = ?",
                params![name],
                |row| row.get(0),
            )
            .optional()?;

        match acl_json {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }

    pub fn set_lock_acl(&self, name: &str, acl: &crate::models::LockAcl) -> Result<()> {
        let conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO lock_acls (name, acquire_acl) VALUES (?, ?)",
            params![name, serde_json::to_string(acl)?],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_user_locks(&self, user_id: Uuid) -> Vec<Lock> {
        self.locks
            .iter()
            .filter_map(|entry| {
                let lock = entry.value();
                if lock.holder_id == user_id && !lock.is_expired() {
                    Some(lock.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn count_user_locks(&self, user_id: Uuid) -> usize {
        self.locks
            .iter()
            .filter(|entry| {
                let lock = entry.value();
                lock.holder_id == user_id && !lock.is_expired()
            })
            .count()
    }

    pub fn get_fencing_counter(&self) -> u64 {
        self.fencing_counter.load(Ordering::SeqCst)
    }

    pub fn list_locks(&self, prefix: Option<&str>) -> Vec<Lock> {
        self.locks
            .iter()
            .filter(|e| prefix.is_none_or(|p| e.key().starts_with(p)))
            .map(|e| e.value().clone())
            .collect()
    }

    pub fn get_all_active_locks(&self) -> Vec<Lock> {
        self.locks
            .iter()
            .filter_map(|entry| {
                let lock = entry.value();
                if !lock.is_expired() {
                    Some(lock.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn release_locks_for_session(&self, session_id: Uuid) {
        let to_remove: Vec<String> = self
            .locks
            .iter()
            .filter(|entry| entry.value().session_id == Some(session_id))
            .map(|entry| entry.key().clone())
            .collect();

        for lock_name in to_remove {
            let dashmap::mapref::entry::Entry::Occupied(entry) =
                self.locks.entry(lock_name.clone())
            else {
                continue;
            };
            if entry.get().session_id != Some(session_id) {
                continue;
            }

            // Keep the entry locked until SQLite accepts the delete. A new
            // lease with the same name cannot be persisted and then erased by
            // this session cleanup.
            if let Err(error) = self.delete_lock_from_database(&lock_name) {
                warn!(
                    "Failed to delete session lock {} from database: {}",
                    lock_name, error
                );
                continue;
            }
            let lock = entry.remove();
            if lock.lock_delay_seconds > 0 {
                let available_at =
                    Utc::now() + chrono::Duration::seconds(lock.lock_delay_seconds as i64);
                self.cooling_locks
                    .insert(lock_name.clone(), (available_at, lock.lock_delay_seconds));
            }
            debug!(
                "Released lock {} (session {} expired)",
                lock_name, session_id
            );
            self.broadcast_event(LockEvent {
                event: LockEventType::Released,
                lock_name,
                lock: None,
                timestamp: Utc::now(),
            });
        }
    }

    pub fn count_session_locks(&self, session_id: Uuid) -> usize {
        self.locks
            .iter()
            .filter(|entry| entry.value().session_id == Some(session_id))
            .count()
    }

    pub fn check_cooling(&self, name: &str) -> Option<(DateTime<Utc>, u32)> {
        if let Some(entry) = self.cooling_locks.get(name) {
            let &(available_at, delay) = entry.value();
            if available_at > Utc::now() {
                return Some((available_at, delay));
            } else {
                drop(entry);
                self.cooling_locks.remove(name);
            }
        }
        None
    }

    fn save_lock_to_database(&self, lock: &Lock) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO locks (name, holder_id, lease_id, fencing_token, expires_at, metadata, acquired_at, session_id, ephemeral, lock_delay_seconds) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                lock.name,
                lock.holder_id.to_string(),
                lock.lease_id.to_string(),
                lock.fencing_token as i64,
                lock.expires_at.to_rfc3339(),
                lock.metadata,
                lock.acquired_at.to_rfc3339(),
                lock.session_id.map(|s| s.to_string()),
                lock.ephemeral as i64,
                lock.lock_delay_seconds as i64,
            ],
        )?;
        Ok(())
    }

    fn delete_lock_from_database(&self, name: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM locks WHERE name = ?", params![name])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;
    use tempfile::NamedTempFile;
    use tokio::time::{sleep, Duration as TokioDuration};

    fn make_db(path: &str) -> DbConn {
        let conn = Connection::open(path).expect("Failed to open DB");
        Arc::new(std::sync::Mutex::new(conn))
    }

    fn create_test_store() -> (LockStore, NamedTempFile) {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let store = LockStore::new(make_db(&db_path), 1).expect("Failed to create store");
        (store, temp_file)
    }

    fn create_test_store_with_path(db_path: &str) -> LockStore {
        LockStore::new(make_db(db_path), 1).expect("Failed to create store")
    }

    #[test]
    fn test_lock_store_enables_wal_mode() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let db = make_db(&db_path);

        let _store = LockStore::new(db.clone(), 1).expect("Failed to create store");

        let conn = db.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("Failed to read journal mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn test_lock_acl_survives_restart() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let expected = crate::models::LockAcl {
            acquire: vec!["user:deploy-bot".to_string()],
        };

        {
            let store = create_test_store_with_path(&db_path);
            store
                .set_lock_acl("deploy/production", &expected)
                .expect("ACL should persist");
        }

        let restored = create_test_store_with_path(&db_path)
            .get_lock_acl("deploy/production")
            .expect("ACL should load")
            .expect("ACL should exist after restart");
        assert_eq!(restored, expected);
    }

    #[tokio::test]
    async fn test_locks_survive_restart_simulation() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        let holder_id = Uuid::new_v4();
        let lock_name = "test-lock-restart".to_string();
        let metadata = Some("test metadata".to_string());

        // Create first store and acquire a lock
        {
            let store1 = create_test_store_with_path(&db_path);
            let result = store1.acquire_lock(
                lock_name.clone(),
                holder_id,
                AcquireLockOptions::new(300).with_metadata(metadata.clone()),
            );
            assert!(result.is_ok());
            let (_lease_id, fencing_token, _) = result.unwrap();
            assert_eq!(fencing_token, 1);

            // Verify the lock is in memory
            let lock = store1.get_lock(&lock_name);
            assert!(lock.is_some());
            assert_eq!(lock.unwrap().holder_id, holder_id);
        } // store1 goes out of scope, simulating restart

        // Create second store from same DB path - should load the lock
        {
            let store2 = create_test_store_with_path(&db_path);

            // Verify the lock was restored from database
            let lock = store2.get_lock(&lock_name);
            assert!(lock.is_some());
            let restored_lock = lock.unwrap();
            assert_eq!(restored_lock.name, lock_name);
            assert_eq!(restored_lock.holder_id, holder_id);
            assert_eq!(restored_lock.fencing_token, 1);
            assert_eq!(restored_lock.metadata, metadata);
            assert!(!restored_lock.is_expired());
        }
    }

    #[tokio::test]
    async fn test_fencing_counter_restores() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        let holder_id1 = Uuid::new_v4();
        let holder_id2 = Uuid::new_v4();

        // Create first store and acquire multiple locks
        {
            let store1 = create_test_store_with_path(&db_path);

            // Acquire first lock (should get fencing token 1)
            let result1 = store1.acquire_lock(
                "lock-1".to_string(),
                holder_id1,
                AcquireLockOptions::new(300),
            );
            assert!(result1.is_ok());
            let (_, fencing_token1, _) = result1.unwrap();
            assert_eq!(fencing_token1, 1);

            // Acquire second lock (should get fencing token 2)
            let result2 = store1.acquire_lock(
                "lock-2".to_string(),
                holder_id2,
                AcquireLockOptions::new(300),
            );
            assert!(result2.is_ok());
            let (_, fencing_token2, _) = result2.unwrap();
            assert_eq!(fencing_token2, 2);

            assert_eq!(store1.get_fencing_counter(), 3);
        }

        // Create second store from same DB - fencing counter should be restored
        {
            let store2 = create_test_store_with_path(&db_path);

            // Fencing counter should be max existing token + 1 = 3
            assert_eq!(store2.get_fencing_counter(), 3);

            // Acquire a new lock - should get fencing token 3
            let result3 = store2.acquire_lock(
                "lock-3".to_string(),
                holder_id1,
                AcquireLockOptions::new(300),
            );
            assert!(result3.is_ok());
            let (_, fencing_token3, _) = result3.unwrap();
            assert_eq!(fencing_token3, 3);
        }
    }

    #[tokio::test]
    async fn test_expired_locks_not_restored() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        let holder_id = Uuid::new_v4();
        let lock_name = "test-lock-expiry".to_string();

        // Create first store and acquire a lock with very short TTL
        {
            let store1 = create_test_store_with_path(&db_path);
            let result =
                store1.acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(1)); // 1 second TTL
            assert!(result.is_ok());

            // Verify lock is initially present
            let lock = store1.get_lock(&lock_name);
            assert!(lock.is_some());
        }

        // Wait for lock to expire
        sleep(TokioDuration::from_secs(2)).await;

        // Create second store - expired lock should not be restored
        {
            let store2 = create_test_store_with_path(&db_path);

            // Expired lock should not be in memory
            let lock = store2.get_lock(&lock_name);
            assert!(lock.is_none());

            // Should be able to acquire the same lock name (it's free)
            let result =
                store2.acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(300));
            assert!(result.is_ok());
        }
    }

    #[test]
    fn test_expired_lock_cannot_be_renewed() {
        let (store, _temp_file) = create_test_store();
        let holder_id = Uuid::new_v4();
        let lock_name = "expired-renewal".to_string();
        let (lease_id, _, _) = store
            .acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(0))
            .expect("initial acquire should succeed");

        let result = store.renew_lock(&lock_name, lease_id, holder_id, 30);
        assert!(matches!(
            result,
            Err(crate::error::AppError::LockNotFound { .. })
        ));
    }

    #[test]
    fn test_stale_expiry_scan_cannot_delete_reacquired_lock() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let store = create_test_store_with_path(&db_path);
        let lock_name = "expiry-race".to_string();

        store
            .acquire_lock(
                lock_name.clone(),
                Uuid::new_v4(),
                AcquireLockOptions::new(0),
            )
            .expect("expired seed lock should be persisted");
        let stale_scan_time = Utc::now();

        let replacement_holder = Uuid::new_v4();
        let (replacement_lease, replacement_term, _) = store
            .acquire_lock(
                lock_name.clone(),
                replacement_holder,
                AcquireLockOptions::new(300),
            )
            .expect("expired lock should be replaceable");

        // Simulate cleanup acting on the name collected by its earlier scan.
        store.cleanup_expired_lock(&lock_name, &stale_scan_time);
        let current = store
            .get_lock(&lock_name)
            .expect("replacement must remain in memory");
        assert_eq!(current.lease_id, replacement_lease);

        drop(store);
        let restored = create_test_store_with_path(&db_path)
            .get_lock(&lock_name)
            .expect("replacement must remain durable across restart");
        assert_eq!(restored.lease_id, replacement_lease);
        assert_eq!(restored.fencing_token, replacement_term);
    }

    #[test]
    fn test_prefix_limit_is_strict_under_concurrent_acquisition() {
        let (store, _temp_file) = create_test_store();
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(16));
        let mut handles = Vec::new();

        for index in 0..16 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.acquire_lock_with_prefix_limit(
                    format!("__election/room-{index}"),
                    Uuid::new_v4(),
                    AcquireLockOptions::new(30),
                    "__election/",
                    1,
                )
            }));
        }

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("campaign thread panicked"))
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(crate::error::AppError::Conflict(_))))
                .count(),
            15
        );
    }

    #[test]
    fn test_prefix_acquisition_returns_an_atomic_leader_snapshot() {
        let (store, _temp_file) = create_test_store();
        let lock_name = "__election/snapshot-room".to_string();
        let leader_id = Uuid::new_v4();
        let (leader_lease, _, _) = store
            .acquire_lock(
                lock_name.clone(),
                leader_id,
                AcquireLockOptions::new(30).with_metadata(Some("leader".to_string())),
            )
            .expect("leader should acquire the room");

        let outcome = store
            .acquire_lock_with_prefix_limit(
                lock_name.clone(),
                Uuid::new_v4(),
                AcquireLockOptions::new(30),
                "__election/",
                1,
            )
            .expect("follower should observe the current leader");
        let AcquireLockOutcome::Held(snapshot) = outcome else {
            panic!("contender should receive a held snapshot");
        };

        store
            .release_lock(&lock_name, leader_lease, leader_id)
            .expect("leader should be able to resign");
        assert_eq!(snapshot.holder_id, leader_id);
        assert_eq!(snapshot.metadata.as_deref(), Some("leader"));
    }

    #[test]
    fn test_renew_returns_snapshot_even_if_lock_is_immediately_released() {
        let (store, _temp_file) = create_test_store();
        let lock_name = "renew-snapshot".to_string();
        let holder_id = Uuid::new_v4();
        let (lease_id, term, _) = store
            .acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(30))
            .expect("leader should acquire the lock");

        let renewed = store
            .renew_lock(&lock_name, lease_id, holder_id, 60)
            .expect("renewal should return the persisted snapshot");
        store
            .release_lock(&lock_name, lease_id, holder_id)
            .expect("release should succeed after renewal");

        assert_eq!(renewed.fencing_token, term);
        assert!(renewed.expires_at > Utc::now());
    }

    #[tokio::test]
    async fn test_release_removes_from_sqlite() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        let holder_id = Uuid::new_v4();
        let lock_name = "test-lock-release".to_string();

        let lease_id;

        // Create first store, acquire and release a lock
        {
            let store1 = create_test_store_with_path(&db_path);
            let result =
                store1.acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(300));
            assert!(result.is_ok());
            let (acquired_lease_id, _, _) = result.unwrap();
            lease_id = acquired_lease_id;

            // Verify lock is present
            let lock = store1.get_lock(&lock_name);
            assert!(lock.is_some());

            // Release the lock
            let release_result = store1.release_lock(&lock_name, lease_id, holder_id);
            assert!(release_result.is_ok());

            // Verify lock is gone from memory
            let lock = store1.get_lock(&lock_name);
            assert!(lock.is_none());
        }

        // Create second store - released lock should not be restored
        {
            let store2 = create_test_store_with_path(&db_path);

            // Released lock should not be in memory
            let lock = store2.get_lock(&lock_name);
            assert!(lock.is_none());

            // Should be able to acquire the same lock name (it's free)
            let result =
                store2.acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(300));
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_metadata_persists() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        let holder_id = Uuid::new_v4();
        let lock_name = "test-lock-metadata".to_string();
        let metadata = Some("important lock metadata with special chars: éñ中文".to_string());

        // Create first store and acquire a lock with metadata
        {
            let store1 = create_test_store_with_path(&db_path);
            let result = store1.acquire_lock(
                lock_name.clone(),
                holder_id,
                AcquireLockOptions::new(300).with_metadata(metadata.clone()),
            );
            assert!(result.is_ok());

            // Verify metadata is correct in memory
            let lock = store1.get_lock(&lock_name);
            assert!(lock.is_some());
            assert_eq!(lock.unwrap().metadata, metadata);
        }

        // Create second store - metadata should be restored
        {
            let store2 = create_test_store_with_path(&db_path);

            // Metadata should be intact after restore
            let lock = store2.get_lock(&lock_name);
            assert!(lock.is_some());
            let restored_lock = lock.unwrap();
            assert_eq!(restored_lock.metadata, metadata);
        }
    }

    #[tokio::test]
    async fn test_multiple_locks_persist() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        let holder_id1 = Uuid::new_v4();
        let holder_id2 = Uuid::new_v4();
        let holder_id3 = Uuid::new_v4();

        let locks_data = vec![
            ("lock-alpha", holder_id1, "metadata for alpha"),
            ("lock-beta", holder_id2, "metadata for beta"),
            ("lock-gamma", holder_id3, "metadata for gamma"),
        ];

        // Create first store and acquire multiple locks
        {
            let store1 = create_test_store_with_path(&db_path);

            for (lock_name, holder_id, metadata) in &locks_data {
                let result = store1.acquire_lock(
                    lock_name.to_string(),
                    *holder_id,
                    AcquireLockOptions::new(300).with_metadata(Some(metadata.to_string())),
                );
                assert!(result.is_ok());
            }

            // Verify all locks are in memory
            assert_eq!(store1.get_all_active_locks().len(), 3);
        }

        // Create second store - all locks should be restored
        {
            let store2 = create_test_store_with_path(&db_path);

            // All locks should be restored
            let all_locks = store2.get_all_active_locks();
            assert_eq!(all_locks.len(), 3);

            // Verify each lock individually
            for (lock_name, expected_holder_id, expected_metadata) in &locks_data {
                let lock = store2.get_lock(lock_name);
                assert!(lock.is_some(), "Lock {} should exist", lock_name);
                let restored_lock = lock.unwrap();
                assert_eq!(restored_lock.holder_id, *expected_holder_id);
                assert_eq!(restored_lock.metadata, Some(expected_metadata.to_string()));
            }
        }
    }

    #[tokio::test]
    async fn test_concurrent_acquire_release_with_persistence() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();

        // Test concurrent operations on the same store
        {
            let store = Arc::new(create_test_store_with_path(&db_path));
            let mut handles = vec![];

            // Spawn multiple threads that acquire and release locks
            for i in 0..10 {
                let store_clone = Arc::clone(&store);
                let handle = thread::spawn(move || {
                    let holder_id = Uuid::new_v4();
                    let lock_name = format!("concurrent-lock-{}", i);

                    // Acquire lock
                    let acquire_result = store_clone.acquire_lock(
                        lock_name.clone(),
                        holder_id,
                        AcquireLockOptions::new(60).with_metadata(Some(format!("thread-{}", i))),
                    );
                    if acquire_result.is_err() {
                        return Err(format!(
                            "Failed to acquire lock {}: {:?}",
                            lock_name,
                            acquire_result.err()
                        ));
                    }

                    let (lease_id, fencing_token, _) = acquire_result.unwrap();

                    // Small delay to simulate work
                    thread::sleep(Duration::from_millis(10));

                    // Release lock
                    let release_result = store_clone.release_lock(&lock_name, lease_id, holder_id);
                    if release_result.is_err() {
                        return Err(format!(
                            "Failed to release lock {}: {:?}",
                            lock_name,
                            release_result.err()
                        ));
                    }

                    Ok(fencing_token)
                });

                handles.push(handle);
            }

            // Collect results
            let mut fencing_tokens = vec![];
            for handle in handles {
                let result = handle.join().expect("Thread panicked");
                assert!(
                    result.is_ok(),
                    "Thread operation failed: {:?}",
                    result.err()
                );
                fencing_tokens.push(result.unwrap());
            }

            // Verify we got unique fencing tokens
            fencing_tokens.sort();
            let expected_tokens: Vec<u64> = (1..=10).collect();
            assert_eq!(fencing_tokens, expected_tokens);

            // Verify no locks remain
            assert_eq!(store.get_all_active_locks().len(), 0);
        }

        // Create second store - should have no locks and correct fencing counter
        {
            let store2 = create_test_store_with_path(&db_path);

            // No locks should be restored (all were released)
            assert_eq!(store2.get_all_active_locks().len(), 0);

            // The next fencing token remains monotonic even when every lock
            // was released before restart.
            assert_eq!(store2.get_fencing_counter(), 11);

            let (_, token, _) = store2
                .acquire_lock(
                    "after-restart".to_string(),
                    Uuid::new_v4(),
                    AcquireLockOptions::new(60),
                )
                .expect("acquire after restart should succeed");
            assert_eq!(token, 11);
        }
    }

    #[tokio::test]
    async fn test_concurrent_same_lock_persistence() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let lock_name = "contested-lock";

        {
            let store = Arc::new(create_test_store_with_path(&db_path));
            let mut handles = vec![];

            // Spawn multiple threads trying to acquire the same lock
            for i in 0..5 {
                let store_clone = Arc::clone(&store);
                let lock_name_clone = lock_name.to_string();
                let handle = thread::spawn(move || {
                    let holder_id = Uuid::new_v4();

                    let acquire_result = store_clone.acquire_lock(
                        lock_name_clone,
                        holder_id,
                        AcquireLockOptions::new(30).with_metadata(Some(format!("contender-{}", i))),
                    );

                    (holder_id, acquire_result)
                });

                handles.push(handle);
            }

            // Collect results - only one should succeed
            let mut successful_acquisitions = 0;
            let mut successful_holder: Option<Uuid> = None;

            for handle in handles {
                let (holder_id, result) = handle.join().expect("Thread panicked");

                if result.is_ok() {
                    successful_acquisitions += 1;
                    successful_holder = Some(holder_id);
                }
            }

            // Exactly one thread should have acquired the lock
            assert_eq!(successful_acquisitions, 1);
            assert!(successful_holder.is_some());

            // Verify the winning lock is in memory
            let lock = store.get_lock(lock_name);
            assert!(lock.is_some());
            assert_eq!(lock.unwrap().holder_id, successful_holder.unwrap());
        }

        // Create second store - the winning lock should be restored
        {
            let store2 = create_test_store_with_path(&db_path);

            let lock = store2.get_lock(lock_name);
            assert!(lock.is_some());
            // The lock should belong to the winning holder from before
            assert!(!lock.unwrap().is_expired());
        }
    }

    use proptest::prelude::*;

    proptest! {
        /// Any lock that is acquired can be released by its owner.
        #[test]
        fn prop_acquired_lock_can_be_released(
            lock_name in "[a-zA-Z0-9.-]{1,50}",
            ttl_seconds in 1u32..3600,
            metadata in prop::option::of("[a-zA-Z0-9_ -]{0,100}")
        ) {
            let (store, _temp_file) = create_test_store();
            let user_id = Uuid::new_v4();

            let (lease_id, _, _) = store
                .acquire_lock(lock_name.clone(), user_id, AcquireLockOptions::new(ttl_seconds).with_metadata(metadata))
                .expect("acquire should succeed");

            store.release_lock(&lock_name, lease_id, user_id)
                .expect("release should succeed for lock owner");

            prop_assert!(store.get_lock(&lock_name).is_none());
        }

        /// Fencing tokens are strictly monotonically increasing across acquires.
        #[test]
        fn prop_fencing_tokens_monotonic(
            count in 2usize..20
        ) {
            let (store, _temp_file) = create_test_store();
            let user_id = Uuid::new_v4();
            let mut tokens = Vec::new();

            for i in 0..count {
                let name = format!("lock-{}", i);
                let (_, token, _) = store
                    .acquire_lock(name, user_id, AcquireLockOptions::new(300))
                    .expect("acquire should succeed");
                tokens.push(token);
            }

            for window in tokens.windows(2) {
                prop_assert!(window[1] > window[0],
                    "fencing tokens must increase: {} should be > {}", window[1], window[0]);
            }
        }

        /// A lock held by user A cannot be released by user B.
        #[test]
        fn prop_lock_owner_isolation(
            lock_name in "[a-zA-Z0-9.-]{1,50}",
            ttl_seconds in 10u32..300
        ) {
            let (store, _temp_file) = create_test_store();
            let user_a = Uuid::new_v4();
            let user_b = Uuid::new_v4();
            prop_assume!(user_a != user_b);

            let (lease_id, _, _) = store
                .acquire_lock(lock_name.clone(), user_a, AcquireLockOptions::new(ttl_seconds))
                .expect("user A acquire should succeed");

            // User B tries to release with the correct lease_id but wrong user
            let result = store.release_lock(&lock_name, lease_id, user_b);
            prop_assert!(result.is_err(), "user B should not be able to release user A's lock");

            // Lock should still belong to user A
            let lock = store.get_lock(&lock_name);
            prop_assert!(lock.is_some());
            prop_assert_eq!(lock.unwrap().holder_id, user_a);
        }

        /// After release, the same lock name can be re-acquired with a higher fencing token.
        #[test]
        fn prop_lock_reacquisition_increments_token(
            lock_name in "[a-zA-Z0-9.-]{1,50}",
            ttl_seconds in 10u32..300
        ) {
            let (store, _temp_file) = create_test_store();
            let user_id = Uuid::new_v4();

            let (lease_id_1, token_1, _) = store
                .acquire_lock(lock_name.clone(), user_id, AcquireLockOptions::new(ttl_seconds))
                .expect("first acquire should succeed");

            store.release_lock(&lock_name, lease_id_1, user_id)
                .expect("release should succeed");

            let (_, token_2, _) = store
                .acquire_lock(lock_name, user_id, AcquireLockOptions::new(ttl_seconds))
                .expect("second acquire should succeed");

            prop_assert!(token_2 > token_1,
                "second fencing token {} must exceed first {}", token_2, token_1);
        }
    }
}
