use crate::{
    error::Result,
    models::{Lock, LockAcl, LockEvent, LockEventType},
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
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

const DEFAULT_WATCH_CHANNEL_LIMIT: usize = 16_384;
const COOLING_SENTINEL_METADATA_PREFIX: &str = "__octostore_cooling_v1:";
// v0.13.2 authenticates its configured admin as the nil UUID and creates every
// ordinary user with `Uuid::new_v4()`. A non-nil, non-v4 UUID therefore remains
// impossible for every principal that the rollback binary can issue while it
// still parses as a holder for the shared on-disk lock schema.
const COOLING_SENTINEL_HOLDER_U128: u128 = u128::MAX;

fn cooling_sentinel_holder_id() -> Uuid {
    Uuid::from_u128(COOLING_SENTINEL_HOLDER_U128)
}

fn cooling_sentinel(
    name: &str,
    available_at: DateTime<Utc>,
    delay: u32,
    fencing_token: u64,
) -> Lock {
    Lock {
        name: name.to_string(),
        holder_id: cooling_sentinel_holder_id(),
        lease_id: Uuid::nil(),
        fencing_token,
        expires_at: available_at,
        metadata: Some(format!("{COOLING_SENTINEL_METADATA_PREFIX}{delay}")),
        acquired_at: available_at,
        session_id: Some(Uuid::nil()),
        ephemeral: false,
        // The rollback binary applies this field after expiry. Keeping it at
        // zero prevents it from adding the same delay a second time.
        lock_delay_seconds: 0,
    }
}

fn has_cooling_sentinel_identity(lock: &Lock) -> bool {
    lock.holder_id == cooling_sentinel_holder_id()
        && lock.lease_id == Uuid::nil()
        && lock.session_id == Some(Uuid::nil())
        && !lock.ephemeral
        && lock.lock_delay_seconds == 0
        && lock.acquired_at == lock.expires_at
}

fn cooling_sentinel_delay(lock: &Lock) -> Option<u32> {
    has_cooling_sentinel_identity(lock).then_some(())?;
    let delay = lock
        .metadata
        .as_deref()?
        .strip_prefix(COOLING_SENTINEL_METADATA_PREFIX)?
        .parse::<u32>()
        .ok()?;
    (delay > 0).then_some(delay)
}

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
    /// Strictly bounded registry of broadcast channels for lock status watchers.
    /// A mutex makes prune-and-admit atomic under concurrent high-cardinality churn.
    watch_channels: Arc<Mutex<HashMap<String, broadcast::Sender<LockEvent>>>>,
    watch_channel_limit: usize,
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
    /// ACL state read and authorized by the caller before entering the store.
    /// When present, acquisition revalidates this snapshot under the per-name
    /// entry guard and atomically persists a requested first ACL with the lock.
    pub acl: Option<AcquireLockAclContext>,
}

#[derive(Debug, Clone)]
pub struct AcquireLockAclContext {
    pub observed: Option<LockAcl>,
    pub requested: Option<LockAcl>,
}

#[derive(Debug, Clone)]
pub enum AcquireLockOutcome {
    Acquired(Lock),
    Held(Lock),
    Delayed {
        available_at: DateTime<Utc>,
        lock_delay_seconds: u32,
    },
}

#[derive(Debug, Clone)]
struct SessionLockSnapshot {
    name: String,
    lease_id: Uuid,
    fencing_token: u64,
    holder_id: Uuid,
    session_id: Uuid,
    ephemeral: bool,
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

    pub fn with_acl_context(
        mut self,
        observed: Option<LockAcl>,
        requested: Option<LockAcl>,
    ) -> Self {
        self.acl = Some(AcquireLockAclContext {
            observed,
            requested,
        });
        self
    }
}

impl LockStore {
    pub fn new(db: DbConn, initial_fencing_token: u64) -> Result<Self> {
        Self::new_with_watch_channel_limit(db, initial_fencing_token, DEFAULT_WATCH_CHANNEL_LIMIT)
    }

    fn new_with_watch_channel_limit(
        db: DbConn,
        initial_fencing_token: u64,
        watch_channel_limit: usize,
    ) -> Result<Self> {
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
            watch_channels: Arc::new(Mutex::new(HashMap::new())),
            watch_channel_limit: watch_channel_limit.max(1),
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
        let mut db = self.db.lock().unwrap();
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
            let session_id = session_id_str
                .map(|value| {
                    Uuid::parse_str(&value).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
                .transpose()?;
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
        let mut loaded_cooling = Vec::new();
        let mut expired_count = 0;
        let mut expired_locks = Vec::new();
        let mut expired_cooling = Vec::new();

        for lock_result in lock_rows {
            let lock = lock_result?;

            if has_cooling_sentinel_identity(&lock) {
                let delay = cooling_sentinel_delay(&lock).ok_or_else(|| {
                    crate::error::AppError::Internal(anyhow::anyhow!(
                        "database contains a malformed cooling sentinel"
                    ))
                })?;
                if lock.expires_at > now {
                    loaded_cooling.push((lock.name, lock.expires_at, delay));
                } else {
                    expired_cooling.push(lock);
                }
            } else if lock.expires_at > now {
                // Lock is still valid, load it into memory
                self.locks.insert(lock.name.clone(), lock);
                loaded_count += 1;
            } else {
                // Convert the expired generation into its durable cooling
                // tombstone after the statement is released.
                expired_locks.push(lock);
                expired_count += 1;
            }
        }
        drop(stmt);

        // Startup cannot omit an expired row from memory and then forget its
        // durable replacement. Real expired locks become sentinel rows in the
        // existing locks table so the rollback binary also treats the name as
        // held until the original delay ends.
        let transaction = db.transaction()?;
        for lock in expired_locks {
            let available_at =
                lock.expires_at + chrono::Duration::seconds(lock.lock_delay_seconds as i64);
            if lock.lock_delay_seconds > 0 && available_at > now {
                let delay = lock.lock_delay_seconds;
                Self::replace_lock_with_cooling_on_connection(
                    &transaction,
                    &lock,
                    available_at,
                    delay,
                )?;
                loaded_cooling.push((lock.name, available_at, delay));
            } else {
                Self::delete_lock_generation_on_connection(&transaction, &lock)?;
            }
        }
        for sentinel in expired_cooling {
            if !Self::delete_cooling_sentinel_on_connection(
                &transaction,
                &sentinel.name,
                sentinel.expires_at,
                cooling_sentinel_delay(&sentinel).expect("classified cooling sentinel"),
            )? {
                return Err(crate::error::AppError::Internal(anyhow::anyhow!(
                    "durable cooling sentinel changed during startup cleanup"
                )));
            }
        }
        transaction.commit()?;

        let loaded_cooling_count = loaded_cooling.len();
        for (name, available_at, delay) in loaded_cooling {
            self.cooling_locks.insert(name, (available_at, delay));
        }

        info!(
            "Loaded {} active locks and {} cooling tombstones from database, cleaned up {} expired locks",
            loaded_count, loaded_cooling_count, expired_count
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
            if let dashmap::mapref::entry::Entry::Occupied(entry) =
                self.cooling_locks.entry(name.clone())
            {
                // The scan is only a hint. A release may have installed a newer
                // delay after it ran, so revalidate under the cooling entry
                // guard before removing anything.
                let available_at = entry.get().0;
                if available_at <= now {
                    let delay = entry.get().1;
                    match self.delete_cooling_if_matches(&name, available_at, delay) {
                        Ok(true) => {
                            entry.remove();
                        }
                        Ok(false) => {}
                        Err(error) => warn!(
                            "Failed to delete expired cooling tombstone {}: {}",
                            name, error
                        ),
                    }
                }
            }
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

    #[cfg(test)]
    pub(crate) async fn cleanup_expired_locks_for_test(&self) {
        self.cleanup_expired_locks().await;
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
        let current_lock = entry.get().clone();
        let lock_delay_seconds = current_lock.lock_delay_seconds;
        let cooling = (lock_delay_seconds > 0).then(|| {
            (
                *now + chrono::Duration::seconds(lock_delay_seconds as i64),
                lock_delay_seconds,
            )
        });
        if let Err(error) = self.delete_lock_and_set_cooling(&current_lock, cooling) {
            warn!(
                "Failed to delete expired lock {} from database: {}",
                lock_name, error
            );
            return;
        }

        if let Some((available_at, delay)) = cooling {
            self.cooling_locks
                .insert(lock_name.to_string(), (available_at, delay));
        }
        // Publish cooling while the occupied lock-name guard is still held.
        // A waiting acquisition cannot observe vacancy before the delay exists.
        let lock = entry.remove();

        debug!("Expired lock: {} (holder: {})", lock_name, lock.holder_id);
        self.broadcast_event(LockEvent {
            event: LockEventType::Expired,
            lock_name: lock_name.to_string(),
            lock: Some(lock),
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
        if let Some(sender) = self
            .watch_channels
            .lock()
            .unwrap()
            .get(&event.lock_name)
            .cloned()
        {
            // We ignore send errors as they just mean no receivers are currently active
            let _ = sender.send(event);
        }
    }

    /// Returns a broadcast receiver for real-time events on a specific lock.
    /// Idle entries are pruned before a new channel is admitted, and the
    /// registry never exceeds its configured hard limit.
    pub fn watch_lock(&self, name: &str) -> Result<broadcast::Receiver<LockEvent>> {
        let mut channels = self.watch_channels.lock().unwrap();
        if let Some(sender) = channels.get(name) {
            return Ok(sender.subscribe());
        }

        channels.retain(|_, sender| sender.receiver_count() > 0);
        if channels.len() >= self.watch_channel_limit {
            return Err(crate::error::AppError::CapacityExceeded {
                details: "Watch registry capacity reached; retry after idle streams close"
                    .to_string(),
                retry_after_seconds: 1,
            });
        }

        let (sender, _) = broadcast::channel(100);
        let receiver = sender.subscribe();
        channels.insert(name.to_string(), sender);
        Ok(receiver)
    }

    // Retained as the tuple-returning compatibility surface for existing
    // internal, benchmark, and downstream callers.
    #[allow(dead_code)]
    pub fn acquire_lock(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
    ) -> Result<(Uuid, u64, DateTime<Utc>)> {
        let lock = self.acquire_lock_snapshot(name, holder_id, options)?;
        Ok((lock.lease_id, lock.fencing_token, lock.expires_at))
    }

    /// Acquires a lock and returns the exact snapshot committed by the store.
    ///
    /// This matters for idempotent same-holder acquisition: requested metadata
    /// does not replace the existing lease metadata, so callers must not build
    /// a response from the request body.
    pub fn acquire_lock_snapshot(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
    ) -> Result<Lock> {
        match self.acquire_lock_outcome(name, holder_id, options)? {
            AcquireLockOutcome::Acquired(lock) => Ok(lock),
            AcquireLockOutcome::Held(_) => Err(crate::error::AppError::LockHeld),
            AcquireLockOutcome::Delayed { .. } => Err(crate::error::AppError::Conflict(
                "Lock is in its configured release delay".to_string(),
            )),
        }
    }

    /// Acquires or renews a lock while atomically enforcing a principal-wide
    /// active-lock limit. `belongs_to_principal` is evaluated while the shared
    /// admission guard is held, so it can resolve current session ownership
    /// without passing a stale session-ID snapshot into the store.
    #[allow(dead_code)]
    pub fn acquire_lock_snapshot_with_principal_limit<F>(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
        max_active: usize,
        belongs_to_principal: F,
    ) -> Result<Lock>
    where
        F: Fn(&Lock) -> bool,
    {
        match self.acquire_lock_outcome_with_principal_limit(
            name,
            holder_id,
            options,
            max_active,
            belongs_to_principal,
        )? {
            AcquireLockOutcome::Acquired(lock) => Ok(lock),
            AcquireLockOutcome::Held(_) => Err(crate::error::AppError::LockHeld),
            AcquireLockOutcome::Delayed { .. } => Err(crate::error::AppError::Conflict(
                "Lock is in its configured release delay".to_string(),
            )),
        }
    }

    pub fn acquire_lock_outcome_with_principal_limit<F>(
        &self,
        name: String,
        holder_id: Uuid,
        options: AcquireLockOptions,
        max_active: usize,
        belongs_to_principal: F,
    ) -> Result<AcquireLockOutcome>
    where
        F: Fn(&Lock) -> bool,
    {
        let _admission = self.admission_guard.lock().unwrap();
        let same_holder_renewal = self
            .locks
            .get(&name)
            .is_some_and(|lock| lock.holder_id == holder_id && !lock.is_expired());

        if !same_holder_renewal {
            let active = self
                .locks
                .iter()
                .filter(|entry| !entry.value().is_expired())
                .filter(|entry| belongs_to_principal(entry.value()))
                .count();
            if active >= max_active {
                return Err(crate::error::AppError::LockLimitExceeded);
            }
        }

        self.acquire_lock_outcome(name, holder_id, options)
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
            acl,
        } = options;

        // Try to insert if not present, or update if held by same user
        match self.locks.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let now = Utc::now();
                if let Some((available_at, lock_delay_seconds)) =
                    self.check_cooling_at(&name, now)?
                {
                    return Ok(AcquireLockOutcome::Delayed {
                        available_at,
                        lock_delay_seconds,
                    });
                }
                let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
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
                self.save_acquired_lock_to_database(&lock, acl.as_ref())?;
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
                // ACL authorization happened before entering the store. Re-read
                // the exact sticky ACL while this name cannot be released,
                // reacquired, or updated through the store. A changed snapshot
                // forces the caller to authorize again instead of observing or
                // acquiring under stale policy.
                self.validate_acquisition_acl(&name, acl.as_ref())?;
                let now = Utc::now();
                let requested_expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
                let existing_lock = entry.get().clone();

                // If an expired generation requested a release delay, expire
                // it under this same name guard before admitting a successor.
                // Otherwise acquisition timing would depend on whether the
                // periodic cleanup happened to run first.
                if existing_lock.expires_at <= now {
                    let expired_delay = existing_lock.lock_delay_seconds;
                    if expired_delay > 0 {
                        let available_at = now + chrono::Duration::seconds(expired_delay as i64);
                        self.delete_lock_and_set_cooling(
                            &existing_lock,
                            Some((available_at, expired_delay)),
                        )?;
                        self.cooling_locks
                            .insert(name.clone(), (available_at, expired_delay));
                        let expired_lock = entry.remove();
                        self.broadcast_event(LockEvent {
                            event: LockEventType::Expired,
                            lock_name: name,
                            lock: Some(expired_lock),
                            timestamp: now,
                        });
                        return Ok(AcquireLockOutcome::Delayed {
                            available_at,
                            lock_delay_seconds: expired_delay,
                        });
                    }

                    let lease_id = Uuid::new_v4();
                    let fencing_token = self.next_fencing_token()?;
                    let lock = Lock {
                        name: name.clone(),
                        holder_id,
                        lease_id,
                        fencing_token,
                        expires_at: requested_expires_at,
                        metadata: metadata.clone(),
                        acquired_at: now,
                        session_id,
                        ephemeral,
                        lock_delay_seconds,
                    };
                    self.save_acquired_lock_to_database(&lock, acl.as_ref())?;
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
                // Same-holder acquisition is an idempotent renewal: keep the
                // lease identity and fencing term, never shorten its lifetime,
                // and make the renewed expiry durable before reporting it.
                else if existing_lock.holder_id == holder_id {
                    let mut renewed_lock = existing_lock.clone();
                    if renewed_lock.expires_at < requested_expires_at {
                        renewed_lock.expires_at = requested_expires_at;
                    }
                    self.save_acquired_lock_to_database(&renewed_lock, acl.as_ref())?;
                    entry.insert(renewed_lock.clone());

                    self.broadcast_event(LockEvent {
                        event: LockEventType::Renewed,
                        lock_name: name.clone(),
                        lock: Some(renewed_lock.clone()),
                        timestamp: now,
                    });

                    Ok(AcquireLockOutcome::Acquired(renewed_lock))
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
                return Err(crate::error::AppError::CapacityExceeded {
                    details: "Public election capacity reached; retry later or self-host OctoStore"
                        .to_string(),
                    retry_after_seconds: 30,
                });
            }
        }

        self.acquire_lock_outcome(name, holder_id, options)
    }

    pub fn release_lock(&self, name: &str, lease_id: Uuid, holder_id: Uuid) -> Result<()> {
        self.release_lock_after_delete(name, lease_id, holder_id, || {})
    }

    fn release_lock_after_delete<F>(
        &self,
        name: &str,
        lease_id: Uuid,
        holder_id: Uuid,
        after_delete: F,
    ) -> Result<()>
    where
        F: FnOnce(),
    {
        match self.locks.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry)
                if entry.get().holder_id == holder_id && entry.get().lease_id == lease_id =>
            {
                if entry.get().is_expired() {
                    return Err(crate::error::AppError::LeaseNotCurrent);
                }
                let current_lock = entry.get().clone();
                let lock_delay = current_lock.lock_delay_seconds;
                let cooling = (lock_delay > 0).then(|| {
                    (
                        Utc::now() + chrono::Duration::seconds(lock_delay as i64),
                        lock_delay,
                    )
                });

                // Durability is part of the API contract. Do not remove the
                // in-memory lease or report success until SQLite atomically
                // replaced it with any required cooling tombstone.
                self.delete_lock_and_set_cooling(&current_lock, cooling)?;
                after_delete();

                // Publish cooling before dropping the occupied name guard. A
                // concurrent acquisition then revalidates this state after it
                // obtains the newly vacant entry.
                if let Some((available_at, delay)) = cooling {
                    self.cooling_locks
                        .insert(name.to_string(), (available_at, delay));
                }
                let released_lock = entry.remove();

                // Broadcast release
                self.broadcast_event(LockEvent {
                    event: LockEventType::Released,
                    lock_name: name.to_string(),
                    lock: Some(released_lock),
                    timestamp: Utc::now(),
                });

                Ok(())
            }
            dashmap::mapref::entry::Entry::Occupied(_) => {
                Err(crate::error::AppError::InvalidLeaseId)
            }
            dashmap::mapref::entry::Entry::Vacant(_) => {
                Err(crate::error::AppError::LeaseNotCurrent)
            }
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
                    return Err(crate::error::AppError::LeaseNotCurrent);
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
            None => Err(crate::error::AppError::LeaseNotCurrent),
        }
    }

    pub fn get_lock(&self, name: &str) -> Option<Lock> {
        self.locks.get(name).map(|entry| entry.value().clone())
    }

    pub fn get_lock_acl(&self, name: &str) -> Result<Option<crate::models::LockAcl>> {
        let conn = self.db.lock().unwrap();
        Self::get_lock_acl_from_connection(&conn, name)
    }

    pub fn set_lock_acl(&self, name: &str, acl: &crate::models::LockAcl) -> Result<()> {
        // ACL-only admin updates and first-ACL seeding participate in the same
        // per-name critical section as acquisition. A vacant entry guard is
        // intentionally retained too: it prevents a first acquisition from
        // authorizing against an absent ACL while this write is in flight.
        match self.locks.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_entry) => self.persist_lock_acl(name, acl),
            dashmap::mapref::entry::Entry::Vacant(_entry) => self.persist_lock_acl(name, acl),
        }
    }

    /// Updates a sticky ACL only if the exact lock generation observed by the
    /// authenticated holder is still current. The occupied entry guard remains
    /// held through SQLite persistence, so release/reacquire cannot turn stale
    /// authorization into a write against a newer holder's ACL.
    pub fn set_lock_acl_for_instance(
        &self,
        name: &str,
        expected: &Lock,
        acl: &LockAcl,
    ) -> Result<()> {
        self.set_lock_acl_for_instance_after_validation(name, expected, acl, || {})
    }

    fn set_lock_acl_for_instance_after_validation<F>(
        &self,
        name: &str,
        expected: &Lock,
        acl: &LockAcl,
        after_validation: F,
    ) -> Result<()>
    where
        F: FnOnce(),
    {
        match self.locks.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                let current = entry.get();
                if current.is_expired()
                    || current.holder_id != expected.holder_id
                    || current.lease_id != expected.lease_id
                    || current.fencing_token != expected.fencing_token
                {
                    return Err(crate::error::AppError::Forbidden(
                        "only current lock holder or admin can update ACL".to_string(),
                    ));
                }
                // Tests pause here to prove this exact occupied-entry guard is
                // retained until persistence finishes. Production uses a no-op
                // hook and pays no synchronization cost.
                after_validation();
                self.persist_lock_acl(name, acl)
            }
            dashmap::mapref::entry::Entry::Vacant(_) => Err(crate::error::AppError::Forbidden(
                "only current lock holder or admin can update ACL".to_string(),
            )),
        }
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

    #[allow(dead_code)]
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

    /// Releases every ephemeral lock for a session and reports durability
    /// failures so callers retain cleanup state instead of acknowledging or
    /// forgetting teardown while a session-bound lease remains live.
    pub fn release_locks_for_session_checked(&self, session_id: Uuid) -> Result<()> {
        self.release_locks_for_session_after_snapshot(session_id, || {})
    }

    fn release_locks_for_session_after_snapshot<F>(
        &self,
        session_id: Uuid,
        after_snapshot: F,
    ) -> Result<()>
    where
        F: FnOnce(),
    {
        // Cleanup scans without holding every shard. Snapshot every field that
        // identifies the exact ephemeral lease, then revalidate the complete
        // identity while holding that name's entry before deleting SQLite.
        let to_remove: Vec<SessionLockSnapshot> = self
            .locks
            .iter()
            .filter_map(|entry| {
                let lock = entry.value();
                let snapshot_session_id = lock.session_id?;
                (snapshot_session_id == session_id && lock.ephemeral).then_some(
                    SessionLockSnapshot {
                        name: entry.key().clone(),
                        lease_id: lock.lease_id,
                        fencing_token: lock.fencing_token,
                        holder_id: lock.holder_id,
                        session_id: snapshot_session_id,
                        ephemeral: lock.ephemeral,
                    },
                )
            })
            .collect();

        after_snapshot();

        let mut first_error = None;
        for snapshot in to_remove {
            let dashmap::mapref::entry::Entry::Occupied(entry) =
                self.locks.entry(snapshot.name.clone())
            else {
                continue;
            };
            let current = entry.get();
            if current.lease_id != snapshot.lease_id
                || current.fencing_token != snapshot.fencing_token
                || current.holder_id != snapshot.holder_id
                || current.session_id != Some(snapshot.session_id)
                || current.ephemeral != snapshot.ephemeral
                || !snapshot.ephemeral
            {
                continue;
            }

            // Keep the entry locked until SQLite accepts the delete. A new
            // lease with the same name cannot be persisted and then erased by
            // this session cleanup.
            let current_lock = entry.get().clone();
            let lock_delay_seconds = current_lock.lock_delay_seconds;
            let cooling = (lock_delay_seconds > 0).then(|| {
                (
                    Utc::now() + chrono::Duration::seconds(lock_delay_seconds as i64),
                    lock_delay_seconds,
                )
            });
            if let Err(error) = self.delete_lock_and_set_cooling(&current_lock, cooling) {
                warn!(
                    "Failed to delete session lock {} from database: {}",
                    snapshot.name, error
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            if let Some((available_at, delay)) = cooling {
                self.cooling_locks
                    .insert(snapshot.name.clone(), (available_at, delay));
            }
            let lock = entry.remove();
            debug!(
                "Released lock {} (session {} expired)",
                snapshot.name, session_id
            );
            self.broadcast_event(LockEvent {
                event: LockEventType::Released,
                lock_name: snapshot.name,
                lock: Some(lock),
                timestamp: Utc::now(),
            });
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn count_session_locks(&self, session_id: Uuid) -> usize {
        self.locks
            .iter()
            .filter(|entry| entry.value().session_id == Some(session_id))
            .count()
    }

    pub fn check_cooling(&self, name: &str) -> Result<Option<(DateTime<Utc>, u32)>> {
        self.check_cooling_at(name, Utc::now())
    }

    fn check_cooling_at(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<(DateTime<Utc>, u32)>> {
        match self.cooling_locks.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                let &(available_at, delay) = entry.get();
                if available_at > now {
                    Ok(Some((available_at, delay)))
                } else {
                    // Revalidate and remove under one cooling-entry guard so a
                    // stale reader cannot erase a newer durable delay for this
                    // name. A mismatch fails closed; it means SQLite changed
                    // before this in-memory generation could be reconciled.
                    if !self.delete_cooling_if_matches(name, available_at, delay)? {
                        return Err(crate::error::AppError::Internal(anyhow::anyhow!(
                            "cooling tombstone changed during expiry cleanup"
                        )));
                    }
                    entry.remove();
                    Ok(None)
                }
            }
            dashmap::mapref::entry::Entry::Vacant(_) => Ok(None),
        }
    }

    fn get_lock_acl_from_connection(conn: &Connection, name: &str) -> Result<Option<LockAcl>> {
        let acl_json: Option<String> = conn
            .query_row(
                "SELECT acquire_acl FROM lock_acls WHERE name = ?",
                params![name],
                |row| row.get(0),
            )
            .optional()?;
        acl_json
            .map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .transpose()
    }

    /// Returns the serialized first ACL that must be inserted with a newly
    /// acquired lock, if any. The caller owns the per-name entry guard.
    fn validate_acquisition_acl_on_connection(
        conn: &Connection,
        name: &str,
        context: Option<&AcquireLockAclContext>,
    ) -> Result<Option<String>> {
        let Some(context) = context else {
            return Ok(None);
        };
        let current = Self::get_lock_acl_from_connection(conn, name)?;
        if current != context.observed {
            return Err(crate::error::AppError::Conflict(
                "ACL changed while acquiring; retry with current policy".to_string(),
            ));
        }
        if let (Some(existing), Some(requested)) = (&current, &context.requested) {
            if existing != requested {
                return Err(crate::error::AppError::Conflict(
                    "ACL already exists; update with PUT /locks/{name}/acl".to_string(),
                ));
            }
        }
        match (&current, &context.requested) {
            (None, Some(requested)) => Ok(Some(serde_json::to_string(requested)?)),
            _ => Ok(None),
        }
    }

    fn validate_acquisition_acl(
        &self,
        name: &str,
        context: Option<&AcquireLockAclContext>,
    ) -> Result<()> {
        let db = self.db.lock().unwrap();
        Self::validate_acquisition_acl_on_connection(&db, name, context).map(|_| ())
    }

    fn save_lock_on_connection(conn: &Connection, lock: &Lock) -> Result<()> {
        conn.execute(
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

    fn save_lock_to_database(&self, lock: &Lock) -> Result<()> {
        let db = self.db.lock().unwrap();
        Self::save_lock_on_connection(&db, lock)
    }

    fn save_acquired_lock_to_database(
        &self,
        lock: &Lock,
        acl: Option<&AcquireLockAclContext>,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let transaction = db.transaction()?;
        let first_acl =
            Self::validate_acquisition_acl_on_connection(&transaction, &lock.name, acl)?;
        Self::save_lock_on_connection(&transaction, lock)?;
        if let Some(acl_json) = first_acl {
            transaction.execute(
                "INSERT INTO lock_acls (name, acquire_acl) VALUES (?, ?)",
                params![lock.name, acl_json],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn persist_lock_acl(&self, name: &str, acl: &LockAcl) -> Result<()> {
        let conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO lock_acls (name, acquire_acl) VALUES (?, ?)",
            params![name, serde_json::to_string(acl)?],
        )?;
        Ok(())
    }

    fn delete_lock_generation_on_connection(conn: &Connection, lock: &Lock) -> Result<()> {
        let changed = conn.execute(
            "DELETE FROM locks WHERE name = ? AND holder_id = ? AND lease_id = ? AND fencing_token = ? AND expires_at = ?",
            params![
                lock.name,
                lock.holder_id.to_string(),
                lock.lease_id.to_string(),
                lock.fencing_token as i64,
                lock.expires_at.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return Err(crate::error::AppError::Internal(anyhow::anyhow!(
                "durable lock generation changed during deletion"
            )));
        }
        Ok(())
    }

    fn replace_lock_with_cooling_on_connection(
        conn: &Connection,
        lock: &Lock,
        available_at: DateTime<Utc>,
        delay: u32,
    ) -> Result<()> {
        let sentinel = cooling_sentinel(&lock.name, available_at, delay, lock.fencing_token);
        let changed = conn.execute(
            "UPDATE locks SET holder_id = ?, lease_id = ?, expires_at = ?, metadata = ?, acquired_at = ?, session_id = ?, ephemeral = ?, lock_delay_seconds = ? WHERE name = ? AND holder_id = ? AND lease_id = ? AND fencing_token = ? AND expires_at = ?",
            params![
                sentinel.holder_id.to_string(),
                sentinel.lease_id.to_string(),
                sentinel.expires_at.to_rfc3339(),
                sentinel.metadata,
                sentinel.acquired_at.to_rfc3339(),
                sentinel.session_id.map(|id| id.to_string()),
                sentinel.ephemeral as i64,
                sentinel.lock_delay_seconds as i64,
                lock.name,
                lock.holder_id.to_string(),
                lock.lease_id.to_string(),
                lock.fencing_token as i64,
                lock.expires_at.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return Err(crate::error::AppError::Internal(anyhow::anyhow!(
                "durable lock generation changed during cooling transition"
            )));
        }
        Ok(())
    }

    fn delete_cooling_sentinel_on_connection(
        conn: &Connection,
        name: &str,
        available_at: DateTime<Utc>,
        delay: u32,
    ) -> Result<bool> {
        Ok(conn.execute(
            "DELETE FROM locks WHERE name = ? AND holder_id = ? AND lease_id = ? AND expires_at = ? AND metadata = ? AND acquired_at = ? AND session_id = ? AND ephemeral = 0 AND lock_delay_seconds = 0",
            params![
                name,
                cooling_sentinel_holder_id().to_string(),
                Uuid::nil().to_string(),
                available_at.to_rfc3339(),
                format!("{COOLING_SENTINEL_METADATA_PREFIX}{delay}"),
                available_at.to_rfc3339(),
                Uuid::nil().to_string(),
            ],
        )? == 1)
    }

    fn delete_lock_and_set_cooling(
        &self,
        lock: &Lock,
        cooling: Option<(DateTime<Utc>, u32)>,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let transaction = db.transaction()?;
        match cooling {
            Some((available_at, delay)) => {
                Self::replace_lock_with_cooling_on_connection(
                    &transaction,
                    lock,
                    available_at,
                    delay,
                )?;
            }
            None => {
                Self::delete_lock_generation_on_connection(&transaction, lock)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn delete_cooling_if_matches(
        &self,
        name: &str,
        available_at: DateTime<Utc>,
        delay: u32,
    ) -> Result<bool> {
        let db = self.db.lock().unwrap();
        Self::delete_cooling_sentinel_on_connection(&db, name, available_at, delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::Duration;
    use tempfile::NamedTempFile;
    use tokio::time::{sleep, Duration as TokioDuration};

    #[test]
    fn principal_lock_limit_admission_is_atomic_across_user_and_session_holders() {
        let (store, _temp_file) = create_test_store();
        let store = Arc::new(store);
        let user_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();

        for index in 0..99 {
            store
                .acquire_lock_snapshot(
                    format!("quota-existing-{index}"),
                    user_id,
                    AcquireLockOptions::new(300),
                )
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(3));
        let attempts = [
            ("quota-user-race", user_id),
            ("quota-session-race", session_id),
        ]
        .into_iter()
        .map(|(name, holder_id)| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                store.acquire_lock_snapshot_with_principal_limit(
                    name.to_string(),
                    holder_id,
                    AcquireLockOptions::new(300)
                        .with_session_id((holder_id == session_id).then_some(session_id))
                        .ephemeral(holder_id == session_id),
                    100,
                    |lock| lock.holder_id == user_id || lock.holder_id == session_id,
                )
            })
        })
        .collect::<Vec<_>>();
        barrier.wait();

        let results = attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(crate::error::AppError::LockLimitExceeded)))
                .count(),
            1
        );
        assert_eq!(
            store
                .list_locks(None)
                .into_iter()
                .filter(|lock| lock.holder_id == user_id || lock.holder_id == session_id)
                .count(),
            100
        );
    }

    #[test]
    fn startup_fails_on_expired_lock_delete_error_and_retries_the_durable_rows() {
        let database = NamedTempFile::new().unwrap();
        let database_path = database.path().to_string_lossy().to_string();
        let session_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        {
            let db = Arc::new(Mutex::new(Connection::open(&database_path).unwrap()));
            let store = LockStore::new(Arc::clone(&db), 0).unwrap();
            store
                .acquire_lock_snapshot(
                    "expired-startup-retry".to_string(),
                    session_id,
                    AcquireLockOptions::new(0)
                        .with_session_id(Some(session_id))
                        .ephemeral(true),
                )
                .unwrap();
            db.lock()
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, ttl_seconds INTEGER NOT NULL, expires_at TEXT NOT NULL, created_at TEXT NOT NULL);\n\
                     INSERT INTO sessions (id, user_id, ttl_seconds, expires_at, created_at) VALUES ('{session_id}', '{user_id}', 60, '{}', '{}');\n\
                     CREATE TRIGGER fail_expired_startup_delete BEFORE DELETE ON locks BEGIN SELECT RAISE(FAIL, 'injected startup delete failure'); END;",
                    (Utc::now() - chrono::Duration::seconds(2)).to_rfc3339(),
                    (Utc::now() - chrono::Duration::seconds(62)).to_rfc3339(),
                ))
                .unwrap();
        }

        let failing_db = Arc::new(Mutex::new(Connection::open(&database_path).unwrap()));
        assert!(matches!(
            LockStore::new(Arc::clone(&failing_db), 0),
            Err(crate::error::AppError::Database(_))
        ));
        assert_eq!(
            failing_db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM locks WHERE name = 'expired-startup-retry'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );
        failing_db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_expired_startup_delete")
            .unwrap();

        let store = LockStore::new(Arc::clone(&failing_db), 0).unwrap();
        let sessions = crate::sessions::SessionStore::new(Arc::clone(&failing_db)).unwrap();
        sessions.reconcile_ephemeral_locks(&store);
        assert!(store.get_lock("expired-startup-retry").is_none());
        assert!(sessions.get_session(session_id).is_none());
        assert_eq!(
            failing_db
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM locks", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn startup_rejects_a_corrupt_persisted_lock_session_identity() {
        let database = NamedTempFile::new().unwrap();
        let database_path = database.path().to_string_lossy().to_string();
        let session_id = Uuid::new_v4();

        {
            let store = create_test_store_with_path(&database_path);
            store
                .acquire_lock_snapshot(
                    "corrupt-session-lock".to_string(),
                    session_id,
                    AcquireLockOptions::new(300)
                        .with_session_id(Some(session_id))
                        .ephemeral(true),
                )
                .unwrap();
        }

        Connection::open(&database_path)
            .unwrap()
            .execute(
                "UPDATE locks SET session_id = 'not-a-uuid' WHERE name = 'corrupt-session-lock'",
                [],
            )
            .unwrap();

        let db = Arc::new(Mutex::new(Connection::open(&database_path).unwrap()));
        assert!(LockStore::new(db, 0).is_err());
    }

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

    #[test]
    fn watch_registry_is_bounded_and_prunes_disconnected_high_cardinality_streams() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let store = LockStore::new_with_watch_channel_limit(make_db(&db_path), 1, 2).unwrap();

        for index in 0..1_000 {
            let receiver = store.watch_lock(&format!("sequential-{index}")).unwrap();
            drop(receiver);
        }
        assert!(store.watch_channels.lock().unwrap().len() <= 2);

        let first = store.watch_lock("active-1").unwrap();
        let second = store.watch_lock("active-2").unwrap();
        assert!(matches!(
            store.watch_lock("rejected"),
            Err(crate::error::AppError::CapacityExceeded { .. })
        ));
        drop(first);
        assert!(store.watch_lock("admitted-after-prune").is_ok());
        drop(second);
    }

    fn create_test_store_with_path(db_path: &str) -> LockStore {
        LockStore::new(make_db(db_path), 1).expect("Failed to create store")
    }

    #[test]
    fn cooling_sentinel_holder_cannot_be_a_v0132_authenticated_principal() {
        let holder_id = cooling_sentinel_holder_id();
        assert!(!holder_id.is_nil(), "v0.13.2 reserves nil for admin");
        assert_ne!(
            holder_id.get_version_num(),
            4,
            "v0.13.2 creates every non-admin principal with Uuid::new_v4()"
        );
    }

    fn assert_durable_cooling_sentinel(
        store: &LockStore,
        name: &str,
        available_at: DateTime<Utc>,
        delay: u32,
    ) {
        let db = store.db.lock().unwrap();
        let matching = db
            .query_row(
                "SELECT COUNT(*) FROM locks WHERE name = ? AND holder_id = ? AND lease_id = ? AND expires_at = ? AND metadata = ? AND acquired_at = ? AND session_id = ? AND ephemeral = 0 AND lock_delay_seconds = 0",
                params![
                    name,
                    cooling_sentinel_holder_id().to_string(),
                    Uuid::nil().to_string(),
                    available_at.to_rfc3339(),
                    format!("{COOLING_SENTINEL_METADATA_PREFIX}{delay}"),
                    available_at.to_rfc3339(),
                    Uuid::nil().to_string(),
                ],
                |row| row.get::<_, u64>(0),
            )
            .unwrap();
        assert_eq!(matching, 1, "cooling must use the exact sentinel shape");
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

    #[test]
    fn initial_lock_and_acl_failure_is_atomic_silent_and_restart_safe() {
        let database = NamedTempFile::new().expect("Failed to create temp file");
        let database_path = database.path().to_string_lossy().to_string();
        let db = make_db(&database_path);
        let store = LockStore::new(Arc::clone(&db), 1).expect("Failed to create store");
        let lock_name = "initial-acl-transaction-failure";
        let mut events = store.watch_lock(lock_name).unwrap();
        db.lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_initial_acl_insert
                 BEFORE INSERT ON lock_acls
                 WHEN NEW.name = 'initial-acl-transaction-failure'
                 BEGIN SELECT RAISE(FAIL, 'injected initial ACL failure'); END;",
            )
            .unwrap();
        let requested_acl = LockAcl {
            acquire: vec!["user:deploy-bot".to_string()],
        };

        let result = store.acquire_lock_snapshot(
            lock_name.to_string(),
            Uuid::new_v4(),
            AcquireLockOptions::new(300).with_acl_context(None, Some(requested_acl.clone())),
        );

        assert!(matches!(result, Err(crate::error::AppError::Database(_))));
        assert!(store.get_lock(lock_name).is_none());
        assert!(store.get_lock_acl(lock_name).unwrap().is_none());
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let conn = db.lock().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM locks WHERE name = ?",
                params![lock_name],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lock_acls WHERE name = ?",
                params![lock_name],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        conn.execute_batch("DROP TRIGGER fail_initial_acl_insert")
            .unwrap();
        drop(conn);
        drop(events);
        drop(store);
        drop(db);

        let restarted = create_test_store_with_path(&database_path);
        assert!(restarted.get_lock(lock_name).is_none());
        assert!(restarted.get_lock_acl(lock_name).unwrap().is_none());
    }

    #[test]
    fn stale_acl_snapshot_cannot_acquire_against_new_sticky_policy() {
        let (store, _database) = create_test_store();
        let lock_name = "stale-acquisition-acl";
        let installed_acl = LockAcl {
            acquire: vec!["user:new-owner".to_string()],
        };
        let stale_requested_acl = LockAcl {
            acquire: vec!["user:stale-owner".to_string()],
        };
        store.set_lock_acl(lock_name, &installed_acl).unwrap();

        let result = store.acquire_lock_snapshot(
            lock_name.to_string(),
            Uuid::new_v4(),
            AcquireLockOptions::new(300).with_acl_context(None, Some(stale_requested_acl)),
        );

        assert!(matches!(result, Err(crate::error::AppError::Conflict(_))));
        assert!(store.get_lock(lock_name).is_none());
        assert_eq!(store.get_lock_acl(lock_name).unwrap(), Some(installed_acl));
    }

    #[test]
    fn release_delay_is_revalidated_after_a_stale_early_check() {
        let (store, _database) = create_test_store();
        let store = Arc::new(store);
        let lock_name = "release-delay-race".to_string();
        let holder_id = Uuid::new_v4();
        let lease = store
            .acquire_lock_snapshot(
                lock_name.clone(),
                holder_id,
                AcquireLockOptions::new(300).with_lock_delay_seconds(3),
            )
            .unwrap();
        let release_deleted = Arc::new(Barrier::new(2));
        let release_resume = Arc::new(Barrier::new(2));
        let releasing_store = Arc::clone(&store);
        let releasing_name = lock_name.clone();
        let deleted = Arc::clone(&release_deleted);
        let resume = Arc::clone(&release_resume);
        let release = thread::spawn(move || {
            releasing_store.release_lock_after_delete(
                &releasing_name,
                lease.lease_id,
                holder_id,
                || {
                    deleted.wait();
                    resume.wait();
                },
            )
        });

        release_deleted.wait();
        assert!(
            store.check_cooling(&lock_name).unwrap().is_none(),
            "the simulated HTTP early check must precede cooling publication"
        );
        let acquiring_store = Arc::clone(&store);
        let acquiring_name = lock_name.clone();
        let (attempted_sender, attempted_receiver) = mpsc::channel();
        let (outcome_sender, outcome_receiver) = mpsc::channel();
        let acquire = thread::spawn(move || {
            assert!(acquiring_store
                .check_cooling(&acquiring_name)
                .unwrap()
                .is_none());
            attempted_sender.send(()).unwrap();
            let outcome = acquiring_store.acquire_lock_outcome(
                acquiring_name,
                Uuid::new_v4(),
                AcquireLockOptions::new(300),
            );
            outcome_sender.send(outcome).unwrap();
        });
        attempted_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("acquisition did not reach its stale early-check boundary");
        assert!(matches!(
            outcome_receiver.recv_timeout(Duration::from_millis(500)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        release_resume.wait();
        assert!(release.join().unwrap().is_ok());
        let outcome = outcome_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("acquisition did not resume after release")
            .unwrap();
        acquire.join().unwrap();
        match outcome {
            AcquireLockOutcome::Delayed {
                available_at,
                lock_delay_seconds,
            } => {
                assert_eq!(lock_delay_seconds, 3);
                assert!(available_at > Utc::now());
            }
            other => panic!("stale early check bypassed release delay: {other:?}"),
        }
        assert!(store.get_lock(&lock_name).is_none());
    }

    #[tokio::test]
    async fn expired_lock_delay_is_identical_before_and_after_periodic_cleanup() {
        let (store, _database) = create_test_store();

        for (lock_name, cleanup_first) in [
            ("expired-delay-direct", false),
            ("expired-delay-cleaned", true),
        ] {
            store
                .acquire_lock_snapshot(
                    lock_name.to_string(),
                    Uuid::new_v4(),
                    AcquireLockOptions::new(0).with_lock_delay_seconds(3),
                )
                .expect("expired seed lock should be committed");

            if cleanup_first {
                store.cleanup_expired_locks_for_test().await;
                assert!(store.get_lock(lock_name).is_none());
            }

            let outcome = store
                .acquire_lock_outcome(
                    lock_name.to_string(),
                    Uuid::new_v4(),
                    AcquireLockOptions::new(300),
                )
                .expect("expired lock should enter its configured delay");
            match outcome {
                AcquireLockOutcome::Delayed {
                    available_at,
                    lock_delay_seconds,
                } => {
                    assert_eq!(lock_delay_seconds, 3);
                    assert!(available_at > Utc::now());
                }
                other => panic!("expired lock bypassed release delay: {other:?}"),
            }
            assert!(store.get_lock(lock_name).is_none());
            let (available_at, delay) = store
                .check_cooling(lock_name)
                .unwrap()
                .expect("expired generation must become durable cooling");
            assert_durable_cooling_sentinel(&store, lock_name, available_at, delay);
        }
    }

    #[test]
    fn release_and_expiry_cooling_survive_restart() {
        for (lock_name, release_first) in [
            ("release-cooling-restart", true),
            ("expiry-cooling-restart", false),
        ] {
            let database = NamedTempFile::new().expect("database fixture");
            let database_path = database.path().to_string_lossy().to_string();
            let expected_available_at = {
                let store = create_test_store_with_path(&database_path);
                let holder_id = Uuid::new_v4();
                let lease = store
                    .acquire_lock_snapshot(
                        lock_name.to_string(),
                        holder_id,
                        AcquireLockOptions::new(if release_first { 300 } else { 0 })
                            .with_lock_delay_seconds(30),
                    )
                    .expect("seed lock should be committed");
                if release_first {
                    store
                        .release_lock(lock_name, lease.lease_id, holder_id)
                        .expect("release should install durable cooling");
                    store
                        .check_cooling(lock_name)
                        .unwrap()
                        .expect("release cooling should be visible")
                        .0
                } else {
                    lease.expires_at + chrono::Duration::seconds(30)
                }
            };

            let restarted = create_test_store_with_path(&database_path);
            let (restored_available_at, restored_delay) = restarted
                .check_cooling(lock_name)
                .unwrap()
                .expect("restart must restore cooling tombstone");
            assert_eq!(restored_available_at, expected_available_at);
            assert_eq!(restored_delay, 30);
            let outcome = restarted
                .acquire_lock_outcome(
                    lock_name.to_string(),
                    Uuid::new_v4(),
                    AcquireLockOptions::new(300),
                )
                .expect("cooling lookup should succeed");
            assert!(matches!(outcome, AcquireLockOutcome::Delayed { .. }));
            assert!(restarted.get_lock(lock_name).is_none());
            assert_durable_cooling_sentinel(
                &restarted,
                lock_name,
                restored_available_at,
                restored_delay,
            );
        }
    }

    #[test]
    fn cooling_persistence_failure_rolls_back_lock_deletion() {
        let (store, _database) = create_test_store();
        let lock_name = "cooling-transaction-failure";
        let holder_id = Uuid::new_v4();
        let lease = store
            .acquire_lock_snapshot(
                lock_name.to_string(),
                holder_id,
                AcquireLockOptions::new(300).with_lock_delay_seconds(30),
            )
            .expect("seed lock should be committed");
        store
            .db
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_cooling_update
                 BEFORE UPDATE ON locks
                 WHEN NEW.name = 'cooling-transaction-failure'
                   AND NEW.metadata LIKE '__octostore_cooling_v1:%'
                 BEGIN SELECT RAISE(FAIL, 'injected cooling update failure'); END;",
            )
            .unwrap();

        assert!(matches!(
            store.release_lock(lock_name, lease.lease_id, holder_id),
            Err(crate::error::AppError::Database(_))
        ));
        assert_eq!(store.get_lock(lock_name).unwrap().lease_id, lease.lease_id);
        assert!(store.check_cooling(lock_name).unwrap().is_none());
        let db = store.db.lock().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM locks WHERE name = ?",
                params![lock_name],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM locks WHERE name = ? AND metadata LIKE '__octostore_cooling_v1:%'",
                params![lock_name],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn cooling_marker_metadata_alone_never_hides_a_real_lock() {
        let database = NamedTempFile::new().expect("database fixture");
        let database_path = database.path().to_string_lossy().to_string();
        let expected_lease = {
            let store = create_test_store_with_path(&database_path);
            store
                .acquire_lock_snapshot(
                    "marker-is-user-metadata".to_string(),
                    Uuid::new_v4(),
                    AcquireLockOptions::new(300)
                        .with_metadata(Some(format!("{COOLING_SENTINEL_METADATA_PREFIX}30"))),
                )
                .unwrap()
                .lease_id
        };

        let restarted = create_test_store_with_path(&database_path);
        assert_eq!(
            restarted
                .get_lock("marker-is-user-metadata")
                .expect("ordinary lock must remain visible")
                .lease_id,
            expected_lease
        );
        assert!(restarted
            .check_cooling("marker-is-user-metadata")
            .unwrap()
            .is_none());
    }

    #[test]
    fn malformed_impossible_cooling_identity_fails_closed_on_restart() {
        let database = NamedTempFile::new().expect("database fixture");
        let database_path = database.path().to_string_lossy().to_string();
        {
            let store = create_test_store_with_path(&database_path);
            let sentinel = cooling_sentinel(
                "malformed-cooling-sentinel",
                Utc::now() + chrono::Duration::seconds(30),
                30,
                7,
            );
            let mut malformed = sentinel;
            malformed.metadata = Some("__octostore_cooling_v1:not-a-number".to_string());
            LockStore::save_lock_on_connection(&store.db.lock().unwrap(), &malformed).unwrap();
        }

        let error = match LockStore::new(make_db(&database_path), 1) {
            Ok(_) => panic!("malformed sentinel must not become an ordinary nil-holder lock"),
            Err(error) => error,
        };
        match error {
            crate::error::AppError::Internal(source) => {
                assert!(source.to_string().contains("malformed cooling sentinel"));
            }
            other => panic!("malformed sentinel returned the wrong error: {other:?}"),
        }
    }

    #[test]
    fn stale_former_holder_cannot_overwrite_reacquired_lock_acl() {
        let database = NamedTempFile::new().expect("Failed to create temp file");
        let database_path = database.path().to_string_lossy().to_string();
        let store = Arc::new(create_test_store_with_path(&database_path));
        let lock_name = "stale-holder-acl-race".to_string();
        let holder_id = Uuid::new_v4();
        let former = store
            .acquire_lock_snapshot(lock_name.clone(), holder_id, AcquireLockOptions::new(300))
            .unwrap();
        let former_lease_id = former.lease_id;
        let stale_acl = LockAcl {
            acquire: vec!["user:former-holder".to_string()],
        };
        let current_acl = LockAcl {
            acquire: vec!["user:new-holder".to_string()],
        };
        let validated = Arc::new(Barrier::new(2));
        let persist = Arc::new(Barrier::new(2));
        let update_store = Arc::clone(&store);
        let update_name = lock_name.clone();
        let update_former = former.clone();
        let update_validated = Arc::clone(&validated);
        let update_persist = Arc::clone(&persist);
        let update = thread::spawn(move || {
            update_store.set_lock_acl_for_instance_after_validation(
                &update_name,
                &update_former,
                &stale_acl,
                || {
                    update_validated.wait();
                    update_persist.wait();
                },
            )
        });

        validated.wait();
        let contender_started = Arc::new(Barrier::new(2));
        let contender_ready = Arc::clone(&contender_started);
        let contender_store = Arc::clone(&store);
        let contender_name = lock_name.clone();
        let replacement_acl = current_acl.clone();
        let (replacement_sender, replacement_receiver) = mpsc::channel();
        let contender = thread::spawn(move || {
            contender_ready.wait();
            contender_store
                .release_lock(&contender_name, former_lease_id, holder_id)
                .unwrap();
            let replacement = contender_store
                .acquire_lock_snapshot(
                    contender_name.clone(),
                    holder_id,
                    AcquireLockOptions::new(300),
                )
                .unwrap();
            contender_store
                .set_lock_acl_for_instance(&contender_name, &replacement, &replacement_acl)
                .unwrap();
            replacement_sender.send(replacement).unwrap();
        });
        contender_started.wait();

        assert!(matches!(
            replacement_receiver.recv_timeout(Duration::from_millis(500)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        persist.wait();
        assert!(update.join().unwrap().is_ok());
        let current = replacement_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("release/reacquire should finish after ACL persistence releases the guard");
        contender.join().unwrap();
        assert_eq!(current.holder_id, holder_id);
        assert_ne!(current.lease_id, former.lease_id);
        assert!(current.fencing_token > former.fencing_token);
        assert!(matches!(
            store.set_lock_acl_for_instance(
                &lock_name,
                &former,
                &LockAcl {
                    acquire: vec!["user:late-stale-holder".to_string()],
                },
            ),
            Err(crate::error::AppError::Forbidden(_))
        ));
        assert_eq!(
            store.get_lock_acl(&lock_name).unwrap(),
            Some(current_acl.clone())
        );
        drop(store);

        let restarted = create_test_store_with_path(&database_path);
        assert_eq!(
            restarted.get_lock_acl(&lock_name).unwrap(),
            Some(current_acl)
        );
        let restored = restarted.get_lock(&lock_name).unwrap();
        assert_eq!(restored.holder_id, holder_id);
        assert_eq!(restored.lease_id, current.lease_id);
        assert_eq!(restored.fencing_token, current.fencing_token);
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
            Err(crate::error::AppError::LeaseNotCurrent)
        ));
    }

    #[test]
    fn same_holder_acquire_persists_and_broadcasts_renewal_without_replacing_lease() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let store = create_test_store_with_path(&db_path);
        let holder_id = Uuid::new_v4();
        let lock_name = "same-holder-renewal".to_string();
        let (lease_id, fencing_token, initial_expiry) = store
            .acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(30))
            .expect("initial acquire should succeed");
        let mut events = store.watch_lock(&lock_name).unwrap();

        let extension_floor = Utc::now() + chrono::Duration::seconds(120);
        let (renewed_lease, renewed_term, renewed_expiry) = store
            .acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(120))
            .expect("same holder should renew");
        assert_eq!(renewed_lease, lease_id);
        assert_eq!(renewed_term, fencing_token);
        assert!(renewed_expiry > initial_expiry);
        assert!(renewed_expiry >= extension_floor);
        let renewed_event = events.try_recv().expect("renewal should be broadcast");
        assert!(matches!(renewed_event.event, LockEventType::Renewed));
        let event_lock = renewed_event.lock.expect("renewal includes lock snapshot");
        assert_eq!(event_lock.lease_id, lease_id);
        assert_eq!(event_lock.fencing_token, fencing_token);
        assert_eq!(event_lock.expires_at, renewed_expiry);

        let (short_lease, short_term, short_expiry) = store
            .acquire_lock(lock_name.clone(), holder_id, AcquireLockOptions::new(1))
            .expect("shorter same-holder request should remain idempotent");
        assert_eq!(short_lease, lease_id);
        assert_eq!(short_term, fencing_token);
        assert_eq!(short_expiry, renewed_expiry, "renewal must never shorten");
        assert!(matches!(
            events
                .try_recv()
                .expect("idempotent renewal is broadcast")
                .event,
            LockEventType::Renewed
        ));

        drop(events);
        drop(store);
        let restored = create_test_store_with_path(&db_path)
            .get_lock(&lock_name)
            .expect("renewed lock should survive restart");
        assert_eq!(restored.lease_id, lease_id);
        assert_eq!(restored.fencing_token, fencing_token);
        assert_eq!(restored.expires_at, renewed_expiry);
    }

    #[test]
    fn session_cleanup_revalidates_snapshot_before_deleting_reacquired_durable_lock() {
        let temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let db_path = temp_file.path().to_string_lossy().to_string();
        let store = Arc::new(create_test_store_with_path(&db_path));
        let session_id = Uuid::new_v4();
        let durable_holder = Uuid::new_v4();
        let lock_name = "session-cleanup-race".to_string();
        let (original_lease, _, _) = store
            .acquire_lock(
                lock_name.clone(),
                session_id,
                AcquireLockOptions::new(300)
                    .with_session_id(Some(session_id))
                    .ephemeral(true),
            )
            .expect("ephemeral session lock should be acquired");

        let snapshot_barrier = Arc::new(Barrier::new(2));
        let resume_barrier = Arc::new(Barrier::new(2));
        let cleanup_store = Arc::clone(&store);
        let cleanup_snapshot_barrier = Arc::clone(&snapshot_barrier);
        let cleanup_resume_barrier = Arc::clone(&resume_barrier);
        let cleanup = thread::spawn(move || {
            cleanup_store
                .release_locks_for_session_after_snapshot(session_id, || {
                    cleanup_snapshot_barrier.wait();
                    cleanup_resume_barrier.wait();
                })
                .unwrap();
        });

        snapshot_barrier.wait();
        store
            .release_lock(&lock_name, original_lease, session_id)
            .expect("original ephemeral lease should release");
        let (replacement_lease, replacement_term, _) = store
            .acquire_lock(
                lock_name.clone(),
                durable_holder,
                AcquireLockOptions::new(300)
                    .with_session_id(Some(session_id))
                    .ephemeral(false),
            )
            .expect("same name should be reacquired as durable");
        assert_ne!(replacement_lease, original_lease);
        resume_barrier.wait();
        cleanup
            .join()
            .expect("session cleanup thread should finish");

        let current = store
            .get_lock(&lock_name)
            .expect("cleanup must preserve the replacement lease");
        assert_eq!(current.lease_id, replacement_lease);
        assert_eq!(current.fencing_token, replacement_term);
        assert_eq!(current.holder_id, durable_holder);
        assert_eq!(current.session_id, Some(session_id));
        assert!(!current.ephemeral);

        drop(store);
        let restored = create_test_store_with_path(&db_path)
            .get_lock(&lock_name)
            .expect("durable replacement should remain in SQLite");
        assert_eq!(restored.lease_id, replacement_lease);
        assert!(!restored.ephemeral);
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
                .filter(|result| {
                    matches!(result, Err(crate::error::AppError::CapacityExceeded { .. }))
                })
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
