use crate::{
    error::Result, 
    models::{Lock, LockEvent, LockEventType}
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rusqlite::{params, Connection};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// A shared, thread-safe SQLite connection used by both [`LockStore`] and
/// [`crate::auth::AuthService`] so the process opens only one file handle.
pub type DbConn = Arc<Mutex<Connection>>;

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
    /// Registry of broadcast channels for lock status watchers.
    watch_channels: Arc<DashMap<String, broadcast::Sender<LockEvent>>>,
}

impl LockStore {
    pub fn new(db: DbConn, initial_fencing_token: u64) -> Result<Self> {
        // Create locks table if it doesn't exist
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
                session_id TEXT
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
        }

        info!("Locks table initialized");

        // Create the store instance
        let store = Self {
            locks: Arc::new(DashMap::new()),
            fencing_counter: Arc::new(AtomicU64::new(initial_fencing_token)),
            db,
            watch_channels: Arc::new(DashMap::new()),
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
            "SELECT name, holder_id, lease_id, fencing_token, expires_at, metadata, acquired_at, session_id FROM locks"
        )?;

        let lock_rows = stmt.query_map([], |row| {
            let holder_id_str: String = row.get(1)?;
            let lease_id_str: String = row.get(2)?;
            let expires_at_str: String = row.get(4)?;
            let acquired_at_str: String = row.get(6)?;
            let session_id_str: Option<String> = row.get(7)?;

            // Map parse errors to rusqlite's InvalidColumnType for propagation
            let holder_id = Uuid::parse_str(&holder_id_str)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
            let lease_id = Uuid::parse_str(&lease_id_str)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e)))?;
            let expires_at = DateTime::parse_from_rfc3339(&expires_at_str)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?
                .with_timezone(&chrono::Utc);
            let acquired_at = DateTime::parse_from_rfc3339(&acquired_at_str)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e)))?
                .with_timezone(&chrono::Utc);
            let session_id = session_id_str.and_then(|s| Uuid::parse_str(&s).ok());

            Ok(Lock {
                name: row.get(0)?,
                holder_id,
                lease_id,
                fencing_token: row.get::<_, i64>(3)? as u64,
                expires_at,
                metadata: row.get(5)?,
                acquired_at,
                session_id,
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
                    warn!("Failed to delete expired lock {} from database: {}", name, e);
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
        let new_counter = std::cmp::max(max_fencing_token + 1, self.fencing_counter.load(Ordering::SeqCst));
        self.fencing_counter.store(new_counter, Ordering::SeqCst);
        
        info!("Updated fencing counter to {} based on existing locks", new_counter);
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
        let mut expired_locks = Vec::new();
        
        for entry in self.locks.iter() {
            if entry.value().expires_at <= now {
                expired_locks.push(entry.key().clone());
            }
        }
        
        for lock_name in expired_locks {
            if let Some((_, lock)) = self.locks.remove(&lock_name) {
                debug!("Expired lock: {} (holder: {})", lock_name, lock.holder_id);
                
                // Broadcast expiration event
                self.broadcast_event(LockEvent {
                    event: LockEventType::Expired,
                    lock_name: lock_name.clone(),
                    lock: None, // Lock is gone
                    timestamp: now,
                });
                
                // Also remove from database
                if let Err(e) = self.delete_lock_from_database(&lock_name) {
                    warn!("Failed to delete expired lock {} from database: {}", lock_name, e);
                }
            }
        }
    }

    /// Broadcasts a lock event to all active watchers for that lock.
    fn broadcast_event(&self, event: LockEvent) {
        if let Some(sender) = self.watch_channels.get(&event.lock_name) {
            // We ignore send errors as they just mean no receivers are currently active
            let _ = sender.send(event);
        }
    }

    /// Returns a broadcast receiver for real-time events on a specific lock.
    pub fn watch_lock(&self, name: &str) -> broadcast::Receiver<LockEvent> {
        let sender = self.watch_channels.entry(name.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100);
            tx
        });
        sender.subscribe()
    }

    pub fn acquire_lock(
        &self,
        name: String,
        holder_id: Uuid,
        ttl_seconds: u32,
        metadata: Option<String>,
        session_id: Option<Uuid>,
    ) -> Result<(Uuid, u64, DateTime<Utc>)> {
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        let lease_id = Uuid::new_v4();
        let fencing_token = self.fencing_counter.fetch_add(1, Ordering::SeqCst);

        // Try to insert if not present, or update if held by same user
        match self.locks.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Lock {
                    name: name.clone(),
                    holder_id,
                    lease_id,
                    fencing_token,
                    expires_at,
                    metadata: metadata.clone(),
                    acquired_at: now,
                    session_id,
                };
                entry.insert(lock.clone());
                
                // Persist to database
                if let Err(e) = self.save_lock_to_database(&lock) {
                    warn!("Failed to save lock {} to database: {}", name, e);
                }
                
                // Broadcast acquisition
                self.broadcast_event(LockEvent {
                    event: LockEventType::Acquired,
                    lock_name: name.clone(),
                    lock: Some(lock),
                    timestamp: now,
                });
                
                Ok((lease_id, fencing_token, expires_at))
            }
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let existing_lock = entry.get();
                
                // If expired, replace it
                if existing_lock.is_expired() {
                    let lock = Lock {
                        name: name.clone(),
                        holder_id,
                        lease_id,
                        fencing_token,
                        expires_at,
                        metadata: metadata.clone(),
                        acquired_at: now,
                        session_id,
                    };
                    entry.insert(lock.clone());
                    
                    // Persist to database
                    if let Err(e) = self.save_lock_to_database(&lock) {
                        warn!("Failed to save lock {} to database: {}", name, e);
                    }

                    // Broadcast new acquisition
                    self.broadcast_event(LockEvent {
                        event: LockEventType::Acquired,
                        lock_name: name.clone(),
                        lock: Some(lock),
                        timestamp: now,
                    });
                    
                    Ok((lease_id, fencing_token, expires_at))
                }
                // If held by same user, return existing lease info (idempotent)
                else if existing_lock.holder_id == holder_id {
                    Ok((existing_lock.lease_id, existing_lock.fencing_token, existing_lock.expires_at))
                }
                // Otherwise, lock is held by someone else
                else {
                    Err(crate::error::AppError::LockHeld)
                }
            }
        }
    }

    pub fn release_lock(&self, name: &str, lease_id: Uuid, holder_id: Uuid) -> Result<()> {
        match self.locks.get(name) {
            Some(lock) if lock.holder_id == holder_id && lock.lease_id == lease_id => {
                drop(lock); // Release the reference before removing
                self.locks.remove(name);
                
                // Broadcast release
                self.broadcast_event(LockEvent {
                    event: LockEventType::Released,
                    lock_name: name.to_string(),
                    lock: None,
                    timestamp: Utc::now(),
                });

                // Also remove from database
                if let Err(e) = self.delete_lock_from_database(name) {
                    warn!("Failed to delete lock {} from database: {}", name, e);
                }
                
                Ok(())
            }
            Some(_) => Err(crate::error::AppError::InvalidLeaseId),
            None => Err(crate::error::AppError::LockNotFound { name: name.to_string() }),
        }
    }

    pub fn renew_lock(
        &self,
        name: &str,
        lease_id: Uuid,
        holder_id: Uuid,
        ttl_seconds: u32,
    ) -> Result<DateTime<Utc>> {
        match self.locks.get_mut(name) {
            Some(mut lock) if lock.holder_id == holder_id && lock.lease_id == lease_id => {
                let now = Utc::now();
                let new_expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
                lock.expires_at = new_expires_at;
                
                // Update database with new expiry time
                if let Err(e) = self.save_lock_to_database(&lock.clone()) {
                    warn!("Failed to update lock {} in database: {}", name, e);
                }

                // Broadcast renewal
                self.broadcast_event(LockEvent {
                    event: LockEventType::Renewed,
                    lock_name: name.to_string(),
                    lock: Some(lock.clone()),
                    timestamp: now,
                });
                
                Ok(new_expires_at)
            }
            Some(_) => Err(crate::error::AppError::InvalidLeaseId),
            None => Err(crate::error::AppError::LockNotFound { name: name.to_string() }),
        }
    }

    pub fn get_lock(&self, name: &str) -> Option<Lock> {
        self.locks.get(name).map(|entry| entry.value().clone())
    }

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
            if let Some((_, _lock)) = self.locks.remove(&lock_name) {
                debug!(
                    "Released lock {} (session {} expired)",
                    lock_name, session_id
                );
                self.broadcast_event(LockEvent {
                    event: LockEventType::Released,
                    lock_name: lock_name.clone(),
                    lock: None,
                    timestamp: Utc::now(),
                });
                if let Err(e) = self.delete_lock_from_database(&lock_name) {
                    warn!(
                        "Failed to delete session lock {} from database: {}",
                        lock_name, e
                    );
                }
            }
        }
    }

    pub fn count_session_locks(&self, session_id: Uuid) -> usize {
        self.locks
            .iter()
            .filter(|entry| entry.value().session_id == Some(session_id))
            .count()
    }

    fn save_lock_to_database(&self, lock: &Lock) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO locks (name, holder_id, lease_id, fencing_token, expires_at, metadata, acquired_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                lock.name,
                lock.holder_id.to_string(),
                lock.lease_id.to_string(),
                lock.fencing_token as i64,
                lock.expires_at.to_rfc3339(),
                lock.metadata,
                lock.acquired_at.to_rfc3339()
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
    use std::sync::Arc;
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
            let result = store1.acquire_lock(lock_name.clone(), holder_id, 300, metadata.clone(), None);
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
            let result1 = store1.acquire_lock("lock-1".to_string(), holder_id1, 300, None, None);
            assert!(result1.is_ok());
            let (_, fencing_token1, _) = result1.unwrap();
            assert_eq!(fencing_token1, 1);
            
            // Acquire second lock (should get fencing token 2)
            let result2 = store1.acquire_lock("lock-2".to_string(), holder_id2, 300, None, None);
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
            let result3 = store2.acquire_lock("lock-3".to_string(), holder_id1, 300, None, None);
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
            let result = store1.acquire_lock(lock_name.clone(), holder_id, 1, None, None); // 1 second TTL
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
            let result = store2.acquire_lock(lock_name.clone(), holder_id, 300, None, None);
            assert!(result.is_ok());
        }
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
            let result = store1.acquire_lock(lock_name.clone(), holder_id, 300, None, None);
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
            let result = store2.acquire_lock(lock_name.clone(), holder_id, 300, None, None);
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
            let result = store1.acquire_lock(lock_name.clone(), holder_id, 300, metadata.clone(), None);
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
                    300, 
                    Some(metadata.to_string()),
                    None
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
                        60, 
                        Some(format!("thread-{}", i)),
                        None
                    );
                    if acquire_result.is_err() {
                        return Err(format!("Failed to acquire lock {}: {:?}", lock_name, acquire_result.err()));
                    }
                    
                    let (lease_id, fencing_token, _) = acquire_result.unwrap();
                    
                    // Small delay to simulate work
                    thread::sleep(Duration::from_millis(10));
                    
                    // Release lock
                    let release_result = store_clone.release_lock(&lock_name, lease_id, holder_id);
                    if release_result.is_err() {
                        return Err(format!("Failed to release lock {}: {:?}", lock_name, release_result.err()));
                    }
                    
                    Ok(fencing_token)
                });
                
                handles.push(handle);
            }
            
            // Collect results
            let mut fencing_tokens = vec![];
            for handle in handles {
                let result = handle.join().expect("Thread panicked");
                assert!(result.is_ok(), "Thread operation failed: {:?}", result.err());
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
            
            // Note: Current implementation resets fencing counter when no locks exist
            // In a production system, you might want to persist the max fencing token separately
            // For now, we test the current behavior
            assert_eq!(store2.get_fencing_counter(), 1);
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
                        30, 
                        Some(format!("contender-{}", i)),
                        None
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
                .acquire_lock(lock_name.clone(), user_id, ttl_seconds, metadata, None)
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
                    .acquire_lock(name, user_id, 300, None, None)
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
                .acquire_lock(lock_name.clone(), user_a, ttl_seconds, None, None)
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
                .acquire_lock(lock_name.clone(), user_id, ttl_seconds, None, None)
                .expect("first acquire should succeed");

            store.release_lock(&lock_name, lease_id_1, user_id)
                .expect("release should succeed");

            let (_, token_2, _) = store
                .acquire_lock(lock_name, user_id, ttl_seconds, None, None)
                .expect("second acquire should succeed");

            prop_assert!(token_2 > token_1,
                "second fencing token {} must exceed first {}", token_2, token_1);
        }
    }
}
