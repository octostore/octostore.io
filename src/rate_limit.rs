use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED_CLIENTS: usize = 10_000;
const OVERFLOW_KEY: &str = "__overflow__";

#[derive(Debug)]
struct ClientWindow {
    started_at: Instant,
    requests: u32,
}

#[derive(Clone, Debug)]
pub struct PublicElectionRateLimiter {
    clients: Arc<Mutex<HashMap<String, ClientWindow>>>,
    max_requests_per_minute: u32,
}

#[derive(Debug, Default)]
struct WatchCounts {
    total: usize,
    by_client: HashMap<String, usize>,
}

#[derive(Clone, Debug)]
pub struct PublicElectionWatchLimiter {
    counts: Arc<Mutex<WatchCounts>>,
    max_total: usize,
    max_per_client: usize,
}

#[derive(Debug)]
pub struct PublicElectionWatchPermit {
    limiter: PublicElectionWatchLimiter,
    client_key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchAdmissionError {
    GlobalCapacity,
    ClientLimit,
}

impl PublicElectionWatchLimiter {
    pub fn new(max_total: usize, max_per_client: usize) -> Self {
        Self {
            counts: Arc::new(Mutex::new(WatchCounts::default())),
            max_total: max_total.max(1),
            max_per_client: max_per_client.max(1),
        }
    }

    pub fn try_acquire(
        &self,
        client_key: &str,
    ) -> Result<PublicElectionWatchPermit, WatchAdmissionError> {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let client_count = counts.by_client.get(client_key).copied().unwrap_or(0);
        if client_count >= self.max_per_client {
            return Err(WatchAdmissionError::ClientLimit);
        }
        if counts.total >= self.max_total {
            return Err(WatchAdmissionError::GlobalCapacity);
        }

        counts.total += 1;
        counts
            .by_client
            .insert(client_key.to_string(), client_count + 1);
        Ok(PublicElectionWatchPermit {
            limiter: self.clone(),
            client_key: client_key.to_string(),
        })
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .total
    }
}

impl Drop for PublicElectionWatchPermit {
    fn drop(&mut self) {
        let mut counts = self
            .limiter
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counts.total = counts.total.saturating_sub(1);
        if let Some(client_count) = counts.by_client.get_mut(&self.client_key) {
            *client_count = client_count.saturating_sub(1);
            if *client_count == 0 {
                counts.by_client.remove(&self.client_key);
            }
        }
    }
}

impl PublicElectionRateLimiter {
    pub fn new(max_requests_per_minute: u32) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            max_requests_per_minute: max_requests_per_minute.max(1),
        }
    }

    /// Limits only election admission traffic. Status, renewal, and resignation
    /// remain available so load cannot prevent a healthy leader from maintaining
    /// or relinquishing its current lease.
    pub fn check(&self, client_key: &str) -> std::result::Result<(), u64> {
        let now = Instant::now();
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let selected_key = if clients.contains_key(client_key) {
            client_key
        } else {
            if clients.len() >= MAX_TRACKED_CLIENTS {
                clients.retain(|_, window| now.duration_since(window.started_at) < WINDOW);
            }
            if clients.len() >= MAX_TRACKED_CLIENTS {
                OVERFLOW_KEY
            } else {
                client_key
            }
        };

        let window = clients
            .entry(selected_key.to_string())
            .or_insert(ClientWindow {
                started_at: now,
                requests: 0,
            });

        let elapsed = now.duration_since(window.started_at);
        if elapsed >= WINDOW {
            window.started_at = now;
            window.requests = 0;
        }

        if window.requests >= self.max_requests_per_minute {
            let remaining = WINDOW.saturating_sub(now.duration_since(window.started_at));
            return Err(remaining.as_secs().max(1));
        }

        window.requests += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_each_client_independently() {
        let limiter = PublicElectionRateLimiter::new(2);
        assert!(limiter.check("198.51.100.1").is_ok());
        assert!(limiter.check("198.51.100.1").is_ok());
        assert!(limiter.check("198.51.100.1").is_err());
        assert!(limiter.check("198.51.100.2").is_ok());
    }

    #[test]
    fn shared_clones_enforce_one_budget() {
        let limiter = PublicElectionRateLimiter::new(1);
        let clone = limiter.clone();
        assert!(limiter.check("203.0.113.7").is_ok());
        assert!(clone.check("203.0.113.7").is_err());
    }

    #[test]
    fn watch_limiter_enforces_per_client_and_global_bounds() {
        let limiter = PublicElectionWatchLimiter::new(2, 1);
        let first = limiter.try_acquire("198.51.100.1").unwrap();
        assert_eq!(
            limiter.try_acquire("198.51.100.1").unwrap_err(),
            WatchAdmissionError::ClientLimit
        );
        let second = limiter.try_acquire("198.51.100.2").unwrap();
        assert_eq!(
            limiter.try_acquire("198.51.100.3").unwrap_err(),
            WatchAdmissionError::GlobalCapacity
        );
        assert_eq!(limiter.active(), 2);

        drop(first);
        assert!(limiter.try_acquire("198.51.100.1").is_ok());
        drop(second);
    }
}
