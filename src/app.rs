use std::sync::Arc;

use crate::auth::AuthService;
use crate::config::Config;
use crate::locks::LockHandlers;
use crate::metrics::Metrics;
use crate::sessions::SessionStore;
use crate::webhooks::WebhookStore;

#[derive(Clone)]
pub struct AppState {
    pub lock_handlers: LockHandlers,
    pub auth_service: AuthService,
    pub config: Config,
    pub metrics: Arc<Metrics>,
    pub session_store: SessionStore,
    pub webhook_store: WebhookStore,
}