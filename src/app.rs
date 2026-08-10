use std::sync::Arc;

use crate::auth::AuthService;
use crate::config::Config;
use crate::locks::LockHandlers;
use crate::metrics::Metrics;
use crate::rate_limit::{PublicElectionRateLimiter, PublicElectionWatchLimiter};
use crate::sessions::SessionStore;
use crate::webhooks::WebhookStore;

#[derive(Clone)]
pub struct AppState {
    pub lock_handlers: LockHandlers,
    pub auth_service: AuthService,
    pub config: Config,
    pub metrics: Arc<Metrics>,
    pub public_election_rate_limiter: PublicElectionRateLimiter,
    pub public_election_watch_limiter: PublicElectionWatchLimiter,
    pub session_store: SessionStore,
    pub webhook_store: WebhookStore,
}
