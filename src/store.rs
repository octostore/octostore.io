use crate::{error::Result, models::Lock};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time;
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct LockStore {
    locks: Arc<DashMap<String, Lock>>,
    fencing_counter: Arc<AtomicU64>,
}

impl LockStore {
    pub fn new(initial_fencing_token: u64) -> Self {
        Self {
            locks: Arc::new(DashMap::new()),
            fencing_counter: Arc::new(AtomicU64::new(initial_fencing_token)),
        }
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
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl_seconds as i64);
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
                };
                entry.insert(lock);
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
                    };
                    entry.insert(lock);
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
}