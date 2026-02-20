use crate::{
    error::{AppError, Result},
    models::{CreateWebhookRequest, LockEvent, LockEventType, Webhook, WebhookResponse},
    store::DbConn,
};
use chrono::Utc;
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use rusqlite::params;
use sha2::Sha256;
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct WebhookStore {
    webhooks: Arc<DashMap<Uuid, Webhook>>,
    db: DbConn,
}

impl WebhookStore {
    pub fn new(db: DbConn) -> Result<Self> {
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
            let events: Vec<String> =
                serde_json::from_str(&events_str).unwrap_or_else(|_| vec!["*".to_string()]);

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
        // Validate URL — must be HTTPS
        if !req.url.starts_with("https://") {
            return Err(AppError::InvalidInput(
                "Webhook URL must use HTTPS".to_string(),
            ));
        }

        // Max 10 webhooks per user
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
            url: req.url,
            secret: req.secret,
            events: req.events.unwrap_or_else(|| vec!["*".to_string()]),
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
        let webhook = self
            .webhooks
            .get(&id)
            .ok_or_else(|| AppError::NotFound("Webhook not found".to_string()))?;

        if webhook.user_id != user_id {
            return Err(AppError::NotFound("Webhook not found".to_string()));
        }

        drop(webhook);
        self.webhooks.remove(&id);

        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM webhooks WHERE id = ?",
            params![id.to_string()],
        )?;

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
        let event_type = match &event.event {
            LockEventType::Acquired => "acquired",
            LockEventType::Released => "released",
            LockEventType::Renewed => "renewed",
            LockEventType::Expired => "expired",
        };

        let holder_id = event.lock.as_ref().map(|l| l.holder_id.to_string());
        let fencing_token = event.lock.as_ref().map(|l| l.fencing_token);

        let payload = serde_json::json!({
            "event": event_type,
            "lock": event.lock_name,
            "holder_id": holder_id,
            "fencing_token": fencing_token,
            "timestamp": event.timestamp.to_rfc3339(),
        });
        let payload_str = payload.to_string();

        // Collect matching webhooks
        let matching: Vec<Webhook> = self
            .webhooks
            .iter()
            .filter_map(|entry| {
                let wh = entry.value();
                if !wh.active {
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
            .collect();

        if matching.is_empty() {
            return;
        }

        // Spawn fire-and-forget delivery task
        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default();

            for webhook in matching {
                let result = deliver(&client, &webhook, &payload_str).await;
                if !result {
                    // Retry once on failure
                    debug!("Retrying webhook {} delivery", webhook.id);
                    deliver(&client, &webhook, &payload_str).await;
                }
            }
        });
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

/// Delivers a single webhook POST. Returns `true` on 2xx, `false` otherwise.
async fn deliver(client: &reqwest::Client, webhook: &Webhook, payload: &str) -> bool {
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
        Err(e) => {
            warn!("Webhook {} delivery error: {}", webhook.id, e);
            false
        }
    }
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
