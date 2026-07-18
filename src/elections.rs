//! Account-free, capability-based leader election over HTTP.
//!
//! Election rooms are public coordination spaces. A successful campaign
//! returns an opaque leader token; possession of that token is the only
//! authority required to renew or resign the lease. No user account or API
//! key is involved.

use crate::{
    error::{AppError, Result},
    models::{validate_metadata, Lock},
    store::{AcquireLockOptions, AcquireLockOutcome},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const ELECTION_PREFIX: &str = "__election/";
const DEFAULT_TTL_SECONDS: u32 = 30;
const MIN_TTL_SECONDS: u32 = 5;
const MAX_TTL_SECONDS: u32 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ElectionMetadata {
    candidate_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CampaignRequest {
    pub candidate_id: String,
    pub ttl_seconds: Option<u32>,
    pub metadata: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LeaderActionRequest {
    pub leader_token: String,
    pub ttl_seconds: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct CreateElectionResponse {
    pub election_id: String,
    pub campaign_path: String,
    pub status_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ElectionLeader {
    pub candidate_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    pub term: u64,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CampaignResponse {
    Leader {
        election_id: String,
        leader: ElectionLeader,
        leader_token: String,
        renew_after_ms: u64,
    },
    Follower {
        election_id: String,
        leader: ElectionLeader,
        retry_after_ms: u64,
    },
}

#[derive(Debug, Serialize)]
pub struct ElectionStatusResponse {
    pub election_id: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leader: Option<ElectionLeader>,
    pub retry_after_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ResignResponse {
    pub election_id: String,
    pub status: &'static str,
    pub previous_term: u64,
}

pub fn is_reserved_lock_name(name: &str) -> bool {
    name.starts_with(ELECTION_PREFIX)
}

fn ensure_enabled(state: &crate::AppState) -> Result<()> {
    if state.config.public_elections_enabled {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "Public elections are disabled on this server".to_string(),
        ))
    }
}

fn validate_election_id(election_id: &str) -> Result<()> {
    if !(8..=64).contains(&election_id.len()) {
        return Err(AppError::InvalidInput(
            "Election ID must be between 8 and 64 characters".to_string(),
        ));
    }
    if !election_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AppError::InvalidInput(
            "Election ID may only contain letters, numbers, '-' and '_'".to_string(),
        ));
    }
    Ok(())
}

fn validate_candidate_id(candidate_id: &str) -> Result<()> {
    if candidate_id.is_empty() || candidate_id.len() > 64 {
        return Err(AppError::InvalidInput(
            "Candidate ID must be between 1 and 64 characters".to_string(),
        ));
    }
    if !candidate_id.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
    }) {
        return Err(AppError::InvalidInput(
            "Candidate ID contains unsupported characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_election_ttl(ttl_seconds: u32) -> Result<()> {
    if !(MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(AppError::InvalidTtl {
            reason: format!(
                "Public election TTL must be between {MIN_TTL_SECONDS} and {MAX_TTL_SECONDS} seconds"
            ),
        });
    }
    Ok(())
}

fn internal_name(election_id: &str) -> String {
    format!("{ELECTION_PREFIX}{election_id}")
}

fn encode_leader_token(holder_id: Uuid, lease_id: Uuid) -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(holder_id.as_bytes());
    bytes[16..].copy_from_slice(lease_id.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_leader_token(token: &str) -> Result<(Uuid, Uuid)> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| AppError::InvalidInput("Invalid leader token".to_string()))?;
    if bytes.len() != 32 {
        return Err(AppError::InvalidInput("Invalid leader token".to_string()));
    }
    let holder_id = Uuid::from_slice(&bytes[..16])?;
    let lease_id = Uuid::from_slice(&bytes[16..])?;
    Ok((holder_id, lease_id))
}

fn metadata_from_lock(lock: &Lock) -> Result<ElectionMetadata> {
    let raw = lock.metadata.as_deref().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "election lock is missing candidate metadata"
        ))
    })?;
    serde_json::from_str(raw).map_err(|error| {
        AppError::Internal(anyhow::anyhow!(
            "election lock metadata is invalid: {error}"
        ))
    })
}

fn leader_from_lock(lock: &Lock) -> Result<ElectionLeader> {
    let metadata = metadata_from_lock(lock)?;
    Ok(ElectionLeader {
        candidate_id: metadata.candidate_id,
        metadata: metadata.metadata,
        term: lock.fencing_token,
        expires_at: lock.expires_at,
    })
}

fn remaining_ms(expires_at: DateTime<Utc>) -> u64 {
    (expires_at - Utc::now()).num_milliseconds().max(0) as u64
}

/// Creates a collision-resistant election room ID. The room itself remains
/// stateless until the first candidate campaigns.
pub async fn create_election(
    State(state): State<crate::AppState>,
) -> Result<(StatusCode, Json<CreateElectionResponse>)> {
    ensure_enabled(&state)?;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let election_id = URL_SAFE_NO_PAD.encode(bytes);

    Ok((
        StatusCode::CREATED,
        Json(CreateElectionResponse {
            campaign_path: format!("/elections/{election_id}/campaign"),
            status_path: format!("/elections/{election_id}"),
            election_id,
        }),
    ))
}

/// Campaigns for leadership without a login. The winner receives a bearer
/// capability that is required for every later mutation of the lease.
pub async fn campaign(
    Path(election_id): Path<String>,
    State(state): State<crate::AppState>,
    Json(request): Json<CampaignRequest>,
) -> Result<Json<CampaignResponse>> {
    ensure_enabled(&state)?;
    validate_election_id(&election_id)?;
    validate_candidate_id(&request.candidate_id)?;
    validate_metadata(&request.metadata)?;

    let ttl_seconds = request.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
    validate_election_ttl(ttl_seconds)?;

    let name = internal_name(&election_id);
    let holder_id = Uuid::new_v4();
    let encoded_metadata = serde_json::to_string(&ElectionMetadata {
        candidate_id: request.candidate_id,
        metadata: request.metadata,
    })?;

    match state.lock_handlers.store.acquire_lock_with_prefix_limit(
        name.clone(),
        holder_id,
        AcquireLockOptions::new(ttl_seconds).with_metadata(Some(encoded_metadata)),
        ELECTION_PREFIX,
        state.config.max_public_elections,
    ) {
        Ok(AcquireLockOutcome::Acquired(lock)) => {
            let leader = leader_from_lock(&lock)?;
            let renew_after_ms = (ttl_seconds as u64 * 1_000) / 2;
            state.metrics.record_lock_operation("acquire");
            Ok(Json(CampaignResponse::Leader {
                election_id,
                leader,
                leader_token: encode_leader_token(holder_id, lock.lease_id),
                renew_after_ms,
            }))
        }
        Ok(AcquireLockOutcome::Held(lock)) => {
            let leader = leader_from_lock(&lock)?;
            Ok(Json(CampaignResponse::Follower {
                election_id,
                retry_after_ms: remaining_ms(leader.expires_at),
                leader,
            }))
        }
        Err(error) => Err(error),
    }
}

pub async fn election_status(
    Path(election_id): Path<String>,
    State(state): State<crate::AppState>,
) -> Result<Json<ElectionStatusResponse>> {
    ensure_enabled(&state)?;
    validate_election_id(&election_id)?;
    let name = internal_name(&election_id);

    match state.lock_handlers.store.get_lock(&name) {
        Some(lock) if !lock.is_expired() => {
            let leader = leader_from_lock(&lock)?;
            Ok(Json(ElectionStatusResponse {
                election_id,
                status: "leader",
                retry_after_ms: remaining_ms(leader.expires_at),
                leader: Some(leader),
            }))
        }
        _ => Ok(Json(ElectionStatusResponse {
            election_id,
            status: "vacant",
            leader: None,
            retry_after_ms: 0,
        })),
    }
}

pub async fn renew_leadership(
    Path(election_id): Path<String>,
    State(state): State<crate::AppState>,
    Json(request): Json<LeaderActionRequest>,
) -> Result<Json<CampaignResponse>> {
    ensure_enabled(&state)?;
    validate_election_id(&election_id)?;
    let ttl_seconds = request.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
    validate_election_ttl(ttl_seconds)?;
    let (holder_id, lease_id) = decode_leader_token(&request.leader_token)?;
    let name = internal_name(&election_id);

    let lock = state
        .lock_handlers
        .store
        .renew_lock(&name, lease_id, holder_id, ttl_seconds)?;
    let leader = leader_from_lock(&lock)?;

    Ok(Json(CampaignResponse::Leader {
        election_id,
        leader,
        leader_token: request.leader_token,
        renew_after_ms: (ttl_seconds as u64 * 1_000) / 2,
    }))
}

pub async fn resign_leadership(
    Path(election_id): Path<String>,
    State(state): State<crate::AppState>,
    Json(request): Json<LeaderActionRequest>,
) -> Result<Json<ResignResponse>> {
    ensure_enabled(&state)?;
    validate_election_id(&election_id)?;
    let (holder_id, lease_id) = decode_leader_token(&request.leader_token)?;
    let name = internal_name(&election_id);
    let previous_term = state
        .lock_handlers
        .store
        .get_lock(&name)
        .map(|lock| lock.fencing_token)
        .ok_or_else(|| AppError::NotFound("Election has no current leader".to_string()))?;

    state
        .lock_handlers
        .store
        .release_lock(&name, lease_id, holder_id)?;
    state.metrics.record_lock_operation("release");

    Ok(Json(ResignResponse {
        election_id,
        status: "vacant",
        previous_term,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthService, config::Config, locks::LockHandlers, metrics::Metrics,
        sessions::SessionStore, store::DbConn, webhooks::WebhookStore,
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::{get, post},
        Router,
    };
    use rusqlite::Connection;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use tempfile::NamedTempFile;
    use tower::ServiceExt;

    async fn test_app_with_limit(max_public_elections: usize) -> (Router, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: temp_file.path().to_string_lossy().to_string(),
            github_client_id: None,
            github_client_secret: None,
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: None,
            admin_username: None,
            static_tokens: None,
            static_tokens_file: None,
            public_elections_enabled: true,
            max_public_elections,
        };
        let db: DbConn = Arc::new(Mutex::new(Connection::open(&config.database_url).unwrap()));
        let auth_service = AuthService::new(config.clone(), db.clone()).unwrap();
        let lock_store = crate::store::LockStore::new(db.clone(), 1).unwrap();
        let session_store = SessionStore::new(db.clone()).unwrap();
        let webhook_store = WebhookStore::new(db).unwrap();
        let state = crate::AppState {
            lock_handlers: LockHandlers::new(lock_store),
            auth_service,
            config,
            metrics: Metrics::new(),
            session_store,
            webhook_store,
        };

        let app = Router::new()
            .route("/elections", post(create_election))
            .route("/elections/:id", get(election_status))
            .route("/elections/:id/campaign", post(campaign))
            .route("/elections/:id/renew", post(renew_leadership))
            .route("/elections/:id/resign", post(resign_leadership))
            .with_state(state);
        (app, temp_file)
    }

    async fn test_app() -> (Router, NamedTempFile) {
        test_app_with_limit(100).await
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn create_room_requires_no_authentication() {
        let (app, _temp) = test_app().await;
        let response = app
            .oneshot(Request::post("/elections").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = json_body(response).await;
        assert_eq!(body["election_id"].as_str().unwrap().len(), 32);
        assert!(body["campaign_path"]
            .as_str()
            .unwrap()
            .starts_with("/elections/"));
    }

    #[tokio::test]
    async fn campaign_renew_resign_and_re_elect() {
        let (app, _temp) = test_app().await;
        let election_id = "release-room-123";

        let first = app
            .clone()
            .oneshot(
                Request::post(format!("/elections/{election_id}/campaign"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"candidate_id":"agent-a","ttl_seconds":30}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first = json_body(first).await;
        assert_eq!(first["status"], "leader");
        assert_eq!(first["leader"]["candidate_id"], "agent-a");
        let first_term = first["leader"]["term"].as_u64().unwrap();
        let leader_token = first["leader_token"].as_str().unwrap().to_string();

        let second = app
            .clone()
            .oneshot(
                Request::post(format!("/elections/{election_id}/campaign"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"candidate_id":"agent-b","ttl_seconds":30}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = json_body(second).await;
        assert_eq!(second["status"], "follower");
        assert_eq!(second["leader"]["candidate_id"], "agent-a");
        assert!(second.get("leader_token").is_none());

        let renewed = app
            .clone()
            .oneshot(
                Request::post(format!("/elections/{election_id}/renew"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"leader_token":leader_token,"ttl_seconds":60}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(renewed.status(), StatusCode::OK);
        let renewed = json_body(renewed).await;
        assert_eq!(renewed["leader"]["term"], first_term);

        let resigned = app
            .clone()
            .oneshot(
                Request::post(format!("/elections/{election_id}/resign"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"leader_token":leader_token}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resigned.status(), StatusCode::OK);

        let replacement = app
            .oneshot(
                Request::post(format!("/elections/{election_id}/campaign"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"candidate_id":"agent-b"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let replacement = json_body(replacement).await;
        assert_eq!(replacement["status"], "leader");
        assert!(replacement["leader"]["term"].as_u64().unwrap() > first_term);
    }

    #[tokio::test]
    async fn invalid_leader_token_cannot_mutate_election() {
        let (app, _temp) = test_app().await;
        let response = app
            .oneshot(
                Request::post("/elections/release-room-123/renew")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"leader_token":"not-a-capability","ttl_seconds":30}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accepts_metadata_at_the_documented_limit() {
        let (app, _temp) = test_app().await;
        let response = app
            .oneshot(
                Request::post("/elections/metadata-room/campaign")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"candidate_id":"agent-a","metadata":"m".repeat(1024)}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_candidate_characters_outside_the_openapi_pattern() {
        let (app, _temp) = test_app().await;
        let response = app
            .oneshot(
                Request::post("/elections/candidate-room/campaign")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"candidate_id":"agent/a"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn public_election_limit_applies_across_rooms() {
        let (app, _temp) = test_app_with_limit(1).await;
        let first = app.clone().oneshot(
            Request::post("/elections/room-one/campaign")
                .header("content-type", "application/json")
                .body(Body::from(json!({"candidate_id":"agent-a"}).to_string()))
                .unwrap(),
        );
        let second = app.oneshot(
            Request::post("/elections/room-two/campaign")
                .header("content-type", "application/json")
                .body(Body::from(json!({"candidate_id":"agent-b"}).to_string()))
                .unwrap(),
        );

        let (first, second) = tokio::join!(first, second);
        let statuses = [first.unwrap().status(), second.unwrap().status()];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::OK)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::CONFLICT)
                .count(),
            1
        );
    }

    #[test]
    fn capability_round_trip() {
        let holder_id = Uuid::new_v4();
        let lease_id = Uuid::new_v4();
        let token = encode_leader_token(holder_id, lease_id);
        assert_eq!(decode_leader_token(&token).unwrap(), (holder_id, lease_id));
        assert_eq!(token.len(), 43);
    }
}
