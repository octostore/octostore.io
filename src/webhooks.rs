use crate::{
    error::{AppError, Result},
    models::{CreateWebhookRequest, LockEvent, LockEventType, Webhook, WebhookResponse},
    store::DbConn,
};
use chrono::Utc;
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use reqwest::Url;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::Sha256;
use std::{
    env,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use uuid::Uuid;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_MAX_CONCURRENT_WEBHOOK_FANOUTS: usize = 32;
const VALID_WEBHOOK_EVENTS: [&str; 5] = ["acquired", "released", "renewed", "expired", "*"];

#[derive(Serialize)]
struct WebhookEventPayload<'a> {
    event: &'a str,
    lock: &'a str,
    holder_id: Option<Uuid>,
    fencing_token: Option<u64>,
    timestamp: String,
}

#[derive(Clone)]
pub struct WebhookStore {
    webhooks: Arc<DashMap<Uuid, Webhook>>,
    db: DbConn,
    mutation_guard: Arc<Mutex<()>>,
    allow_private_networks: bool,
    fanout_permits: Arc<Semaphore>,
}

impl WebhookStore {
    pub fn new(db: DbConn) -> Result<Self> {
        Self::new_with_fanout_limit(db, DEFAULT_MAX_CONCURRENT_WEBHOOK_FANOUTS)
    }

    fn new_with_fanout_limit(db: DbConn, max_concurrent_fanouts: usize) -> Result<Self> {
        {
            let conn = db.lock().unwrap();
            conn.execute(
                r#"
                CREATE TABLE IF NOT EXISTS webhooks (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    url TEXT NOT NULL,
                    secret TEXT,
                    events TEXT NOT NULL,
                    lock_pattern TEXT,
                    created_at TEXT NOT NULL,
                    active INTEGER NOT NULL DEFAULT 1
                )
                "#,
                [],
            )?;
        }

        info!("Webhooks table initialized");

        let store = Self {
            webhooks: Arc::new(DashMap::new()),
            db,
            mutation_guard: Arc::new(Mutex::new(())),
            allow_private_networks: matches!(
                env::var("OCTOSTORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS").as_deref(),
                Ok("1" | "true")
            ),
            fanout_permits: Arc::new(Semaphore::new(max_concurrent_fanouts)),
        };

        store.load_webhooks_from_database()?;

        Ok(store)
    }

    fn load_webhooks_from_database(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT id, user_id, url, secret, events, lock_pattern, created_at, active FROM webhooks WHERE active = 1",
        )?;

        let rows = stmt.query_map([], |row| {
            let id_str: String = row.get(0)?;
            let user_id_str: String = row.get(1)?;
            let events_str: String = row.get(4)?;
            let created_at_str: String = row.get(6)?;
            let active: bool = row.get::<_, i64>(7)? != 0;

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
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&chrono::Utc);
            let events: Vec<String> = serde_json::from_str(&events_str).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            if !events
                .iter()
                .all(|event| VALID_WEBHOOK_EVENTS.contains(&event.as_str()))
            {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "stored webhook contains an unsupported event name",
                    )),
                ));
            }

            Ok(Webhook {
                id,
                user_id,
                url: row.get(2)?,
                secret: row.get(3)?,
                events,
                lock_pattern: row.get(5)?,
                created_at,
                active,
            })
        })?;

        let mut count = 0;
        for row in rows {
            let webhook = row?;
            self.webhooks.insert(webhook.id, webhook);
            count += 1;
        }

        info!("Loaded {} active webhooks from database", count);
        Ok(())
    }

    pub fn create_webhook(&self, user_id: Uuid, req: CreateWebhookRequest) -> Result<Webhook> {
        let url = Url::parse(&req.url).map_err(|_| {
            AppError::InvalidInput("Webhook URL must be a valid HTTPS URL".to_string())
        })?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err(AppError::InvalidInput(
                "Webhook URL must use HTTPS".to_string(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::InvalidInput(
                "Webhook URL must not contain credentials".to_string(),
            ));
        }
        validate_literal_webhook_destination(&url, self.allow_private_networks)?;
        let events = req.events.unwrap_or_else(|| vec!["*".to_string()]);
        validate_webhook_events(&events)?;

        // Admission and persistence are serialized so concurrent requests
        // cannot all observe the same remaining quota.
        let _mutation = self.mutation_guard.lock().unwrap();
        let user_count = self
            .webhooks
            .iter()
            .filter(|entry| entry.value().user_id == user_id)
            .count();
        if user_count >= 10 {
            return Err(AppError::InvalidInput(
                "Maximum 10 webhooks per user".to_string(),
            ));
        }

        let webhook = Webhook {
            id: Uuid::new_v4(),
            user_id,
            url: url.to_string(),
            secret: req.secret,
            events,
            lock_pattern: req.lock_pattern,
            created_at: Utc::now(),
            active: true,
        };

        self.save_webhook_to_database(&webhook)?;
        self.webhooks.insert(webhook.id, webhook.clone());

        info!("Webhook created: {} for user {}", webhook.id, user_id);
        Ok(webhook)
    }

    pub fn delete_webhook(&self, id: Uuid, user_id: Uuid) -> Result<()> {
        let _mutation = self.mutation_guard.lock().unwrap();
        let webhook = self
            .webhooks
            .get(&id)
            .ok_or_else(|| AppError::NotFound("Webhook not found".to_string()))?;

        if webhook.user_id != user_id {
            return Err(AppError::NotFound("Webhook not found".to_string()));
        }

        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM webhooks WHERE id = ?", params![id.to_string()])?;
        drop(db);
        drop(webhook);
        self.webhooks.remove(&id);

        info!("Webhook deleted: {}", id);
        Ok(())
    }

    pub fn get_user_webhooks(&self, user_id: Uuid) -> Vec<Webhook> {
        self.webhooks
            .iter()
            .filter_map(|entry| {
                let wh = entry.value();
                if wh.user_id == user_id {
                    Some(wh.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Fire-and-forget dispatch of a lock event to all matching webhooks.
    pub fn dispatch(&self, event: &LockEvent) {
        self.dispatch_with(
            event,
            |webhook, payload, allow_private_networks| async move {
                deliver(&webhook, payload.as_ref(), allow_private_networks).await
            },
        );
    }

    fn dispatch_with<D, F>(&self, event: &LockEvent, deliverer: D)
    where
        D: Fn(Webhook, Arc<str>, bool) -> F + Send + 'static,
        F: Future<Output = bool> + Send + 'static,
    {
        let event_type = match &event.event {
            LockEventType::Acquired => "acquired",
            LockEventType::Released => "released",
            LockEventType::Renewed => "renewed",
            LockEventType::Expired => "expired",
        };

        let Some(owner_id) = self.event_owner_id(event) else {
            debug!(
                lock = %event.lock_name,
                "Skipping webhook event without a resolvable authorized owner"
            );
            return;
        };
        let payload_str = webhook_payload(event, event_type);

        // Collect matching webhooks
        let matching = self.matching_webhooks(event, event_type, owner_id);

        if matching.is_empty() {
            return;
        }

        // Admission happens before spawning so both queued and actively
        // delivering fan-outs share one application-wide hard bound. Saturation is
        // an expected best-effort drop and intentionally emits no per-event log.
        let Ok(fanout_permit) = Arc::clone(&self.fanout_permits).try_acquire_owned() else {
            return;
        };

        let allow_private_networks = self.allow_private_networks;
        let payload_str: Arc<str> = payload_str.into();
        tokio::spawn(async move {
            let _fanout_permit = fanout_permit;
            for webhook in matching {
                let result = deliverer(
                    webhook.clone(),
                    Arc::clone(&payload_str),
                    allow_private_networks,
                )
                .await;
                if !result {
                    // Retry once on failure
                    debug!("Retrying webhook {} delivery", webhook.id);
                    deliverer(webhook, Arc::clone(&payload_str), allow_private_networks).await;
                }
            }
        });
    }

    fn matching_webhooks(
        &self,
        event: &LockEvent,
        event_type: &str,
        owner_id: Uuid,
    ) -> Vec<Webhook> {
        self.webhooks
            .iter()
            .filter_map(|entry| {
                let wh = entry.value();
                if !wh.active || wh.user_id != owner_id {
                    return None;
                }
                // Check event type match
                if !wh.events.contains(&"*".to_string())
                    && !wh.events.contains(&event_type.to_string())
                {
                    return None;
                }
                // Check lock_pattern match
                if let Some(ref pattern) = wh.lock_pattern {
                    if !matches_pattern(pattern, &event.lock_name) {
                        return None;
                    }
                }
                Some(wh.clone())
            })
            .collect()
    }

    fn event_owner_id(&self, event: &LockEvent) -> Option<Uuid> {
        let lock = event.lock.as_ref()?;
        if !lock.ephemeral {
            return Some(lock.holder_id);
        }

        let session_id = lock.session_id?;
        let db = self.db.lock().unwrap();
        let owner_id = db
            .query_row(
                "SELECT user_id FROM sessions WHERE id = ?",
                params![session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()?;
        Uuid::parse_str(&owner_id).ok()
    }

    fn save_webhook_to_database(&self, webhook: &Webhook) -> Result<()> {
        let db = self.db.lock().unwrap();
        let events_json = serde_json::to_string(&webhook.events)?;
        db.execute(
            "INSERT OR REPLACE INTO webhooks (id, user_id, url, secret, events, lock_pattern, created_at, active) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                webhook.id.to_string(),
                webhook.user_id.to_string(),
                webhook.url,
                webhook.secret,
                events_json,
                webhook.lock_pattern,
                webhook.created_at.to_rfc3339(),
                webhook.active as i64,
            ],
        )?;
        Ok(())
    }
}

fn validate_webhook_events(events: &[String]) -> Result<()> {
    if events
        .iter()
        .all(|event| VALID_WEBHOOK_EVENTS.contains(&event.as_str()))
    {
        Ok(())
    } else {
        Err(AppError::InvalidInput(
            "Webhook events must be acquired, released, renewed, expired, or *".to_string(),
        ))
    }
}

fn webhook_payload(event: &LockEvent, event_type: &str) -> String {
    serde_json::to_string(&WebhookEventPayload {
        event: event_type,
        lock: &event.lock_name,
        holder_id: event
            .lock
            .as_ref()
            .map(|lock| crate::locks::public_holder_id(lock.holder_id)),
        fencing_token: event.lock.as_ref().map(|lock| lock.fencing_token),
        timestamp: event.timestamp.to_rfc3339(),
    })
    .expect("webhook event payload fields are infallible to serialize")
}

/// Simple glob matching: `"*"` matches everything, `"prefix*"` matches names
/// starting with `prefix`, otherwise exact match.
fn matches_pattern(pattern: &str, lock_name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        lock_name.starts_with(prefix)
    } else {
        pattern == lock_name
    }
}

fn sign_payload(secret: &str, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    let result = mac.finalize();
    format!("sha256={}", hex::encode(result.into_bytes()))
}

fn public_webhook_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_webhook_ipv4(address),
        IpAddr::V6(address) => public_webhook_ipv6(address),
    }
}

fn public_webhook_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0)
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn public_webhook_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_webhook_ipv4(mapped);
    }
    let segments = address.segments();
    let global_unicast = (0x2000..=0x3fff).contains(&segments[0]);
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    global_unicast
        && !documentation
        && !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
}

fn validate_literal_webhook_destination(url: &Url, allow_private: bool) -> Result<()> {
    if allow_private {
        return Ok(());
    }
    let host = url.host_str().unwrap_or_default();
    let rejected = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| !public_webhook_ip(address));
    if rejected {
        Err(AppError::InvalidInput(
            "Webhook URL must resolve only to public network addresses".to_string(),
        ))
    } else {
        Ok(())
    }
}

async fn resolve_webhook_destination(
    url: &Url,
    allow_private: bool,
) -> std::result::Result<(String, Vec<SocketAddr>), &'static str> {
    let host = url.host_str().ok_or("missing_host")?.to_string();
    let port = url.port_or_known_default().ok_or("missing_port")?;
    let mut addresses = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| "dns_resolution_failed")?
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err("dns_resolution_empty");
    }
    if !resolved_webhook_addresses_allowed(&addresses, allow_private) {
        return Err("private_destination_rejected");
    }
    Ok((host, addresses))
}

fn resolved_webhook_addresses_allowed(addresses: &[SocketAddr], allow_private: bool) -> bool {
    !addresses.is_empty()
        && (allow_private
            || addresses
                .iter()
                .all(|address| public_webhook_ip(address.ip())))
}

/// Delivers a single webhook POST. DNS is resolved and pinned for every
/// attempt so a hostname cannot pass validation and then rebind to a private IP.
async fn deliver(webhook: &Webhook, payload: &str, allow_private: bool) -> bool {
    let url = match Url::parse(&webhook.url) {
        Ok(url) => url,
        Err(_) => return false,
    };
    let (host, addresses) = match resolve_webhook_destination(&url, allow_private).await {
        Ok(destination) => destination,
        Err(error_class) => {
            warn!(webhook_id = %webhook.id, error_class, "Webhook destination rejected");
            return false;
        }
    };
    let client = match client_builder_with_trust().and_then(|builder| {
        builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .resolve_to_addrs(&host, &addresses)
            .build()
            .map_err(|error| format!("could not build webhook HTTP client: {error}"))
    }) {
        Ok(client) => client,
        Err(error) => {
            warn!(webhook_id = %webhook.id, error_class = "client_build", "{error}");
            return false;
        }
    };
    let mut request = client
        .post(&webhook.url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "OctoStore-Webhook/1.0");

    if let Some(ref secret) = webhook.secret {
        let signature = sign_payload(secret, payload);
        request = request.header("X-OctoStore-Signature", signature);
    }

    match request.body(payload.to_string()).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                debug!("Webhook {} delivered ({})", webhook.id, status);
                true
            } else {
                warn!("Webhook {} delivery failed ({})", webhook.id, status);
                false
            }
        }
        Err(error) => {
            log_delivery_error(webhook.id, &error);
            false
        }
    }
}

fn delivery_error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "transport"
    }
}

fn log_delivery_error(webhook_id: Uuid, error: &reqwest::Error) {
    warn!(
        webhook_id = %webhook_id,
        error_class = delivery_error_class(error),
        "Webhook delivery transport error"
    );
}

fn client_builder_with_trust() -> std::result::Result<reqwest::ClientBuilder, String> {
    let mut builder = reqwest::Client::builder().use_rustls_tls();
    let Some(path) = env::var_os("OCTOSTORE_CA_BUNDLE") else {
        return Ok(builder);
    };
    let certificates = std::fs::read(&path).map_err(|error| {
        format!(
            "could not read OCTOSTORE_CA_BUNDLE {}: {error}",
            std::path::Path::new(&path).display()
        )
    })?;
    let certificates = reqwest::Certificate::from_pem_bundle(&certificates)
        .map_err(|error| format!("could not parse OCTOSTORE_CA_BUNDLE: {error}"))?;
    if certificates.is_empty() {
        return Err("OCTOSTORE_CA_BUNDLE contains no certificates".to_string());
    }
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    Ok(builder)
}

// ── Route handlers ──────────────────────────────────────────────────────

pub async fn create_webhook_handler(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<WebhookResponse>)> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let webhook = state.webhook_store.create_webhook(user_id, req)?;

    Ok((StatusCode::CREATED, Json(WebhookResponse::from(webhook))))
}

pub async fn list_webhooks(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<WebhookResponse>>> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let webhooks = state.webhook_store.get_user_webhooks(user_id);
    let responses: Vec<WebhookResponse> = webhooks.into_iter().map(WebhookResponse::from).collect();

    Ok(Json(responses))
}

pub async fn delete_webhook_handler(
    Path(id): Path<Uuid>,
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<StatusCode> {
    let user_id = state.auth_service.authenticate(&headers)?;

    state.webhook_store.delete_webhook(id, user_id)?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{Lock, LockEvent},
        sessions::SessionStore,
    };
    use chrono::Duration as ChronoDuration;
    use rusqlite::Connection;
    use serde_json::Value;
    use std::{
        io::Write,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Barrier, Mutex,
        },
        thread,
        time::Duration as StdDuration,
    };
    use tracing_subscriber::fmt::MakeWriter;

    fn test_store() -> (WebhookStore, SessionStore) {
        test_store_with_fanout_limit(DEFAULT_MAX_CONCURRENT_WEBHOOK_FANOUTS)
    }

    fn test_store_with_fanout_limit(max_concurrent_fanouts: usize) -> (WebhookStore, SessionStore) {
        let db = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let webhooks =
            WebhookStore::new_with_fanout_limit(Arc::clone(&db), max_concurrent_fanouts).unwrap();
        let sessions = SessionStore::new(db).unwrap();
        (webhooks, sessions)
    }

    async fn wait_until(condition: impl Fn() -> bool) {
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not satisfied before the test deadline");
    }

    fn request(url: &str, pattern: &str) -> CreateWebhookRequest {
        CreateWebhookRequest {
            url: url.to_string(),
            secret: None,
            events: Some(vec!["acquired".to_string()]),
            lock_pattern: Some(pattern.to_string()),
        }
    }

    fn event(name: &str, holder_id: Uuid, session_id: Option<Uuid>, ephemeral: bool) -> LockEvent {
        let now = Utc::now();
        LockEvent {
            event: LockEventType::Acquired,
            lock_name: name.to_string(),
            lock: Some(Lock {
                name: name.to_string(),
                holder_id,
                lease_id: Uuid::new_v4(),
                fencing_token: 17,
                expires_at: now + ChronoDuration::seconds(60),
                metadata: None,
                acquired_at: now,
                session_id,
                ephemeral,
                lock_delay_seconds: 0,
            }),
            timestamp: now,
        }
    }

    #[test]
    fn webhook_urls_are_parsed_structurally_and_require_https() {
        let (store, _sessions) = test_store();
        let user_id = Uuid::new_v4();
        for invalid in [
            "https://",
            "https:// example.com/hook",
            "http://example.com/hook",
            "https://user:secret@example.com/hook",
            "https://localhost/hook",
            "https://127.0.0.1/hook",
            "https://[::1]/hook",
            "https://169.254.169.254/latest/meta-data",
            "not-a-url",
        ] {
            assert!(matches!(
                store.create_webhook(user_id, request(invalid, "*")),
                Err(AppError::InvalidInput(_))
            ));
        }
        assert!(store
            .create_webhook(
                user_id,
                request("https://example.com/hook?token=opaque", "*")
            )
            .is_ok());
    }

    #[test]
    fn webhook_event_names_are_validated_before_persistence() {
        let (store, _sessions) = test_store();
        let user_id = Uuid::new_v4();
        let allowed = VALID_WEBHOOK_EVENTS
            .iter()
            .map(|event| (*event).to_string())
            .collect::<Vec<_>>();
        let mut all_events = request("https://events.example/all", "*");
        all_events.events = Some(allowed.clone());
        let created = store.create_webhook(user_id, all_events).unwrap();
        assert_eq!(created.events, allowed);

        let mut default_events = request("https://events.example/default", "*");
        default_events.events = None;
        let created = store.create_webhook(user_id, default_events).unwrap();
        assert_eq!(created.events, vec!["*"]);

        for (index, invalid) in ["", "ACQUIRED", "acquire", "acquired,*", "deleted"]
            .into_iter()
            .enumerate()
        {
            let mut invalid_request =
                request(&format!("https://events-{index}.example/invalid"), "*");
            invalid_request.events = Some(vec!["acquired".to_string(), invalid.to_string()]);
            assert!(matches!(
                store.create_webhook(user_id, invalid_request),
                Err(AppError::InvalidInput(_))
            ));
        }

        assert_eq!(store.get_user_webhooks(user_id).len(), 2);
        let persisted: i64 = store
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM webhooks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(persisted, 2);
    }

    #[test]
    fn malformed_or_unsupported_persisted_events_fail_closed_on_restart() {
        for events in ["not-json", r#"["acquired","deleted"]"#] {
            let db = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
            let store = WebhookStore::new(Arc::clone(&db)).unwrap();
            drop(store);

            db.lock()
                .unwrap()
                .execute(
                    "INSERT INTO webhooks (id, user_id, url, events, created_at, active) VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                    params![
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        "https://events.example/restart",
                        events,
                        Utc::now().to_rfc3339(),
                    ],
                )
                .unwrap();

            assert!(WebhookStore::new(Arc::clone(&db)).is_err());
        }
    }

    #[test]
    fn webhook_dns_policy_rejects_private_and_mixed_rebinding_answers() {
        let public = "93.184.216.34:443".parse::<SocketAddr>().unwrap();
        let private = "10.0.0.8:443".parse::<SocketAddr>().unwrap();
        let link_local = "[fe80::1]:443".parse::<SocketAddr>().unwrap();
        assert!(resolved_webhook_addresses_allowed(&[public], false));
        assert!(!resolved_webhook_addresses_allowed(&[private], false));
        assert!(!resolved_webhook_addresses_allowed(
            &[public, private],
            false
        ));
        assert!(!resolved_webhook_addresses_allowed(&[link_local], false));
        assert!(resolved_webhook_addresses_allowed(&[private], true));
    }

    #[test]
    fn webhook_quota_is_atomic_under_concurrent_creation() {
        let (store, _sessions) = test_store();
        let owner = Uuid::new_v4();
        for index in 0..9 {
            store
                .create_webhook(
                    owner,
                    request(&format!("https://seed-{index}.example/hook"), "*"),
                )
                .unwrap();
        }

        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(9));
        let attempts = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store.create_webhook(
                        owner,
                        request(&format!("https://race-{index}.example/hook"), "*"),
                    )
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();

        let successes = attempts
            .into_iter()
            .filter_map(|attempt| attempt.join().unwrap().ok())
            .count();
        assert_eq!(successes, 1);
        assert_eq!(store.get_user_webhooks(owner).len(), 10);
        let persisted: i64 = store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM webhooks WHERE user_id = ?",
                params![owner.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted, 10);
    }

    #[test]
    fn failed_webhook_delete_keeps_runtime_and_durable_state_aligned() {
        let (store, _sessions) = test_store();
        let owner = Uuid::new_v4();
        let webhook = store
            .create_webhook(owner, request("https://delete.example/hook", "*"))
            .unwrap();
        store
            .db
            .lock()
            .unwrap()
            .execute_batch(
                r#"
                CREATE TRIGGER fail_webhook_delete
                BEFORE DELETE ON webhooks
                BEGIN
                    SELECT RAISE(ABORT, 'injected webhook delete failure');
                END;
                "#,
            )
            .unwrap();

        assert!(store.delete_webhook(webhook.id, owner).is_err());
        assert_eq!(store.get_user_webhooks(owner).len(), 1);
        let persisted: i64 = store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM webhooks WHERE id = ?",
                params![webhook.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted, 1);
    }

    #[test]
    fn webhook_delivery_selection_is_tenant_scoped_and_hides_session_capabilities() {
        let (store, sessions) = test_store();
        let owner = Uuid::new_v4();
        let other_user = Uuid::new_v4();
        let owner_webhook = store
            .create_webhook(owner, request("https://owner.example/hook", "shared-*"))
            .unwrap();
        let other_webhook = store
            .create_webhook(
                other_user,
                request("https://other.example/hook", "shared-*"),
            )
            .unwrap();
        let session = sessions.create_session(owner, Some(60)).unwrap();
        let session_event = event("shared-session-lock", session.id, Some(session.id), true);

        let resolved_owner = store.event_owner_id(&session_event).unwrap();
        assert_eq!(resolved_owner, owner);
        let selected = store.matching_webhooks(&session_event, "acquired", resolved_owner);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, owner_webhook.id);
        assert_ne!(selected[0].id, other_webhook.id);

        let payload: Value =
            serde_json::from_str(&webhook_payload(&session_event, "acquired")).unwrap();
        assert_ne!(payload["holder_id"], session.id.to_string());
        assert!(Uuid::parse_str(payload["holder_id"].as_str().unwrap()).is_ok());

        let other_event = event("shared-other-lock", other_user, None, false);
        let selected = store.matching_webhooks(
            &other_event,
            "acquired",
            store.event_owner_id(&other_event).unwrap(),
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, other_webhook.id);
        assert_ne!(selected[0].id, owner_webhook.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn webhook_fanout_live_delivery_never_exceeds_global_bound() {
        const LIMIT: usize = 3;
        let (store, _sessions) = test_store_with_fanout_limit(LIMIT);
        let owner = Uuid::new_v4();
        store
            .create_webhook(owner, request("https://bounded.example/hook", "*"))
            .unwrap();
        let lock_event = event("bounded-lock", owner, None, false);

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let release_gate = Arc::new(Semaphore::new(0));
        let deliverer = {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let started = Arc::clone(&started);
            let release_gate = Arc::clone(&release_gate);
            move |_webhook: Webhook, _payload: Arc<str>, _allow_private_networks: bool| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let started = Arc::clone(&started);
                let release_gate = Arc::clone(&release_gate);
                async move {
                    let live = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(live, Ordering::SeqCst);
                    started.fetch_add(1, Ordering::SeqCst);
                    let release = release_gate.acquire_owned().await.unwrap();
                    release.forget();
                    active.fetch_sub(1, Ordering::SeqCst);
                    true
                }
            }
        };

        for _ in 0..(LIMIT * 4) {
            store.dispatch_with(&lock_event, deliverer.clone());
        }
        wait_until(|| started.load(Ordering::SeqCst) == LIMIT).await;
        assert_eq!(active.load(Ordering::SeqCst), LIMIT);
        assert_eq!(peak.load(Ordering::SeqCst), LIMIT);
        assert_eq!(store.fanout_permits.available_permits(), 0);

        let saturated_logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(CapturedWriter(Arc::clone(&saturated_logs)))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..100 {
                store.dispatch_with(&lock_event, deliverer.clone());
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), LIMIT);
        assert!(saturated_logs.lock().unwrap().is_empty());

        release_gate.add_permits(LIMIT);
        wait_until(|| active.load(Ordering::SeqCst) == 0).await;
        wait_until(|| store.fanout_permits.available_permits() == LIMIT).await;
        assert_eq!(started.load(Ordering::SeqCst), LIMIT);

        store.dispatch_with(&lock_event, deliverer);
        wait_until(|| started.load(Ordering::SeqCst) == LIMIT + 1).await;
        assert_eq!(peak.load(Ordering::SeqCst), LIMIT);
        release_gate.add_permits(1);
        wait_until(|| active.load(Ordering::SeqCst) == 0).await;
        wait_until(|| store.fanout_permits.available_permits() == LIMIT).await;
    }

    #[derive(Clone)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    struct CapturedGuard(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedGuard {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedWriter {
        type Writer = CapturedGuard;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedGuard(Arc::clone(&self.0))
        }
    }

    #[tokio::test]
    async fn webhook_transport_logs_never_include_url_credentials() {
        let sentinel = "SENTINEL_WEBHOOK_PATH_AND_QUERY_SECRET";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest::Client::new()
            .post(format!("http://{address}/{sentinel}?token={sentinel}"))
            .send()
            .await
            .unwrap_err();

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(CapturedWriter(Arc::clone(&bytes)))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            log_delivery_error(Uuid::new_v4(), &error);
        });

        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(output.contains("error_class"));
        assert!(!output.contains(sentinel));
        assert!(!output.contains(&address.to_string()));
    }
}
