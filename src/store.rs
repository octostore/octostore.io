use crate::{error::Result, models::Lock};
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
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Clone)]
pub struct LockStore {
    locks: Arc<DashMap<String, Lock>>,
    fencing_counter: Arc<AtomicU64>,
    db: Arc<Mutex<Connection>>,
}

impl LockStore {
    pub fn new(database_url: &str, initial_fencing_token: u64) -> Result<Self> {
        // Create database connection
        let conn = Connection::open(database_url)?;
        
        // Create locks table if it doesn't exist
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS locks (
                name TEXT PRIMARY KEY,
                holder_id TEXT NOT NULL,
                lease_id TEXT NOT NULL,
                fencing_token INTEGER NOT NULL,
                expires_at TEXT NOT NULL,
                metadata TEXT,
                acquired_at TEXT NOT NULL
            )
            "#,
            [],
        )?;
        
        info!("Locks table initialized");
        
        // Create the store instance
        let store = Self {
            locks: Arc::new(DashMap::new()),
            fencing_counter: Arc::new(AtomicU64::new(initial_fencing_token)),
            db: Arc::new(Mutex::new(conn)),
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
            "SELECT name, holder_id, lease_id, fencing_token, expires_at, metadata, acquired_at FROM locks"
        )?;
        
        let lock_rows = stmt.query_map([], |row| {
            let expires_at_str: String = row.get(4)?;
            let acquired_at_str: String = row.get(6)?;
            
            Ok(Lock {
                name: row.get(0)?,
                holder_id: Uuid::parse_str(&row.get::<_, String>(1)?).unwrap(),
                lease_id: Uuid::parse_str(&row.get::<_, String>(2)?).unwrap(),
                fencing_token: row.get::<_, i64>(3)? as u64,
                expires_at: chrono::DateTime::parse_from_rfc3339(&expires_at_str)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                metadata: row.get(5)?,
                acquired_at: chrono::DateTime::parse_from_rfc3339(&acquired_at_str)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
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
        
        // Set fencing counter to max + 1
        let new_counter = max_fencing_token + 1;
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
                
                // Also remove from database
                if let Err(e) = self.delete_lock_from_database(&lock_name) {
                    warn!("Failed to delete expired lock {} from database: {}", lock_name, e);
                }
            }
        }
    }

    pub fn acquire_lock(
        &self,
        name: String,
        holder_id: Uuid,
        ttl_seconds: u32,
        metadata: Option<String>,
    ) -> Result<(Uuid, u64, DateTime<Utc>)> {
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
        let lease_id = Uuid::new_v4();
        let fencing_token = self.fencing_counter.fetch_add(1, Ordering::SeqCst) + 1;

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
                };
                entry.insert(lock.clone());
                
                // Persist to database
                if let Err(e) = self.save_lock_to_database(&lock) {
                    warn!("Failed to save lock {} to database: {}", name, e);
                    // Continue anyway - DashMap is the source of truth
                }
                
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
                    };
                    entry.insert(lock.clone());
                    
                    // Persist to database
                    if let Err(e) = self.save_lock_to_database(&lock) {
                        warn!("Failed to save lock {} to database: {}", name, e);
                        // Continue anyway - DashMap is the source of truth
                    }
                    
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
                
                // Also remove from database
                if let Err(e) = self.delete_lock_from_database(name) {
                    warn!("Failed to delete lock {} from database: {}", name, e);
                    // Continue anyway - DashMap removal was successful
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
                let new_expires_at = Utc::now() + chrono::Duration::seconds(ttl_seconds as i64);
                lock.expires_at = new_expires_at;
                
                // Update database with new expiry time
                if let Err(e) = self.save_lock_to_database(&lock.clone()) {
                    warn!("Failed to update lock {} in database: {}", name, e);
                    // Continue anyway - DashMap update was successful
                }
                
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