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
}
