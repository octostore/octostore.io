use crate::{
    config::{parse_static_token_list, Config},
    error::{AppError, Result},
    models::{AuthTokenResponse, GitHubTokenResponse, GitHubUser, User},
};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::Engine;
use chrono::Utc;
use dashmap::DashMap;
use rand::{rngs::OsRng, Rng, RngCore};
use reqwest::Client;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{debug, info};
use uuid::Uuid;

const OAUTH_STATE_COOKIE: &str = "__Host-octostore-oauth-state";
const OAUTH_STATE_TTL: Duration = Duration::from_secs(5 * 60);
const OAUTH_HANDOFF_TTL: Duration = Duration::from_secs(60);
const MAX_PENDING_OAUTH_FLOWS: usize = 10_000;
const IDENTITY_SOURCE_LEGACY: &str = "legacy";
const IDENTITY_SOURCE_LOCAL: &str = "local";
const IDENTITY_SOURCE_OAUTH: &str = "oauth";
const IDENTITY_SOURCE_STATIC: &str = "static";

struct OAuthHandoff {
    response: AuthTokenResponse,
    expires_at: Instant,
}

struct StoredIdentity {
    id: String,
    github_id: i64,
    username: String,
    token: String,
    namespace: Option<String>,
    created_at: String,
    identity_source: String,
}

// ---------------------------------------------------------------------------
// Local-auth helpers
// ---------------------------------------------------------------------------

/// Derive a stable numeric ID from a username so local users fit the existing
/// `github_id INTEGER UNIQUE` schema.  FNV-1a is deterministic and needs no
/// extra crate.  The high bit is set to keep these well above real GitHub IDs.
fn username_to_local_id(username: &str) -> u64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut hash = FNV_OFFSET;
    for byte in username.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash | (1u64 << 63) // separate namespace from real GitHub IDs
}

// ---------------------------------------------------------------------------
// AuthService
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AuthService {
    pub db: crate::store::DbConn,
    http_client: Client,
    config: Config,
    /// Cache: token → user_id (avoids SQLite mutex on every request)
    token_cache: Arc<DashMap<String, Uuid>>,
    /// Browser-bound, one-time OAuth state values.
    oauth_states: Arc<Mutex<HashMap<String, Instant>>>,
    /// Short-lived one-time codes used to hand API credentials to the dashboard.
    oauth_handoffs: Arc<Mutex<HashMap<String, OAuthHandoff>>>,
    #[cfg(test)]
    github_token_endpoint: String,
    #[cfg(test)]
    github_user_endpoint: String,
}

#[derive(Deserialize)]
pub struct GitHubCallbackQuery {
    code: String,
    state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthExchangeRequest {
    pub exchange_code: String,
}

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub namespace: Option<String>,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub token: String,
    pub user_id: Uuid,
    pub username: String,
    pub namespace: Option<String>,
}

impl AuthService {
    pub fn new(config: Config, db: crate::store::DbConn) -> Result<Self> {
        {
            let conn = db.lock().unwrap();

            conn.execute(
                r#"
            CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                github_id INTEGER NOT NULL UNIQUE,
                github_username TEXT NOT NULL,
                token TEXT NOT NULL UNIQUE,
                namespace TEXT,
                identity_source TEXT NOT NULL DEFAULT 'legacy'
                    CHECK (identity_source IN ('legacy', 'local', 'oauth', 'static')),
                created_at TEXT NOT NULL
            )
            "#,
                [],
            )?;

            let _ = conn.execute("ALTER TABLE users ADD COLUMN namespace TEXT", []);
            if conn
                .prepare("SELECT identity_source FROM users LIMIT 0")
                .is_err()
            {
                conn.execute(
                    "ALTER TABLE users ADD COLUMN identity_source TEXT NOT NULL DEFAULT 'legacy' \
                     CHECK (identity_source IN ('legacy', 'local', 'oauth', 'static'))",
                    [],
                )?;
            }

            // Existing builds used the high bit to separate local/static IDs
            // from real GitHub IDs. Preserve established OAuth sessions while
            // leaving ambiguous negative-ID rows inactive in OAuth mode until
            // their source is proven by configuration or a fresh OAuth login.
            conn.execute(
                "UPDATE users SET identity_source = 'oauth' \
                 WHERE identity_source = 'legacy' AND github_id >= 0",
                [],
            )?;

            // ACL user principals are case-insensitive, so authentication must
            // never materialize two case variants as distinct bearer users.
            // Creating the invariant at startup also fails closed on unsafe
            // legacy databases instead of loading ambiguous identities.
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS users_username_nocase_unique \
                 ON users(github_username COLLATE NOCASE)",
                [],
            )?;

            conn.execute(
                r#"
            CREATE TABLE IF NOT EXISTS fencing_counter (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                counter INTEGER NOT NULL DEFAULT 0
            )
            "#,
                [],
            )?;

            conn.execute(
                "INSERT OR IGNORE INTO fencing_counter (id, counter) VALUES (1, 0)",
                [],
            )?;

            // Older builds materialized the synthetic admin principal as a
            // normal user with a fixed bearer token. The nil UUID is an
            // in-process authorization sentinel only: it must never be a
            // durable user or enter the ordinary bearer-token path.
            conn.execute(
                "DELETE FROM users WHERE id = ?",
                params![Uuid::nil().to_string()],
            )?;

            info!("Database initialized at: {}", config.database_url);
        }

        // Pre-load existing tokens into cache
        let token_cache = DashMap::new();
        {
            let conn = db.lock().unwrap();
            let mut stmt =
                conn.prepare("SELECT token, id, identity_source FROM users WHERE id <> ?")?;
            let rows = stmt.query_map(params![Uuid::nil().to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for (token, id, identity_source) in rows.flatten() {
                if !Self::identity_source_is_active(&config, &identity_source) {
                    continue;
                }
                if let Ok(uuid) = Uuid::parse_str(&id) {
                    if !uuid.is_nil() {
                        token_cache.insert(token, uuid);
                    }
                }
            }
            info!("Loaded {} tokens into auth cache", token_cache.len());
        }

        Ok(Self {
            db,
            http_client: Client::new(),
            config,
            token_cache: Arc::new(token_cache),
            oauth_states: Arc::new(Mutex::new(HashMap::new())),
            oauth_handoffs: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            github_token_endpoint: "https://github.com/login/oauth/access_token".to_string(),
            #[cfg(test)]
            github_user_endpoint: "https://api.github.com/user".to_string(),
        })
    }

    fn identity_source_is_active(config: &Config, identity_source: &str) -> bool {
        if config.is_github_enabled() {
            identity_source == IDENTITY_SOURCE_OAUTH
        } else {
            matches!(
                identity_source,
                IDENTITY_SOURCE_LEGACY | IDENTITY_SOURCE_LOCAL | IDENTITY_SOURCE_STATIC
            )
        }
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by the binary's full HTTP-flow tests, not the library test target.
    pub(crate) fn with_github_endpoints(
        mut self,
        token_endpoint: String,
        user_endpoint: String,
    ) -> Self {
        self.github_token_endpoint = token_endpoint;
        self.github_user_endpoint = user_endpoint;
        self
    }

    // -----------------------------------------------------------------------
    // Local-auth: static token seeding (fully synchronous)
    // -----------------------------------------------------------------------

    /// Seed tokens from configured sources. Any read, parse, or persistence
    /// failure is returned so startup cannot silently use a different auth set.
    pub fn seed_static_tokens(&self) -> Result<()> {
        let mut pairs: Vec<(String, String)> = Vec::new();

        if let Some(raw) = &self.config.static_tokens {
            pairs
                .extend(parse_static_token_list(raw, "STATIC_TOKENS").map_err(AppError::Internal)?);
        }

        if let Some(path) = &self.config.static_tokens_file {
            let contents = std::fs::read_to_string(path).map_err(|error| {
                AppError::Internal(anyhow::anyhow!(
                    "failed to read configured STATIC_TOKENS_FILE {path}: {error}"
                ))
            })?;
            pairs.extend(
                parse_static_token_list(&contents, "STATIC_TOKENS_FILE")
                    .map_err(AppError::Internal)?,
            );
        }

        let mut configured_usernames = HashSet::new();
        let mut configured_ids = HashSet::new();
        let mut configured_tokens = HashSet::new();
        for (username, token) in &pairs {
            let normalized_username = username.to_ascii_lowercase();
            let local_id = username_to_local_id(username) as i64;
            if !configured_usernames.insert(normalized_username)
                || !configured_ids.insert(local_id)
                || !configured_tokens.insert(token.clone())
            {
                return Err(AppError::Conflict(
                    "configured static identities are ambiguous or duplicated".to_string(),
                ));
            }
        }

        enum SeedAction {
            Existing {
                user_id: Uuid,
                token: String,
            },
            Insert {
                user_id: Uuid,
                local_id: i64,
                username: String,
                token: String,
                created_at: String,
            },
        }

        let mut conn = self.db.lock().unwrap();
        let transaction = conn.transaction()?;
        let mut actions = Vec::with_capacity(pairs.len());

        // Validate the complete desired set and every possible uniqueness
        // conflict before issuing the first INSERT.
        for (username, token) in pairs {
            let local_id = username_to_local_id(&username) as i64;
            let mut statement = transaction.prepare(
                "SELECT id, github_id, github_username, token, identity_source FROM users \
                 WHERE github_id = ? OR github_username = ? COLLATE NOCASE OR token = ?",
            )?;
            let matches = statement
                .query_map(params![local_id, username, token], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(statement);

            match matches.as_slice() {
                [] => actions.push(SeedAction::Insert {
                    user_id: Uuid::new_v4(),
                    local_id,
                    username,
                    token,
                    created_at: Utc::now().to_rfc3339(),
                }),
                [(stored_id, stored_local_id, stored_username, stored_token, stored_source)]
                    if *stored_local_id == local_id
                        && stored_username.eq_ignore_ascii_case(&username)
                        && stored_token == &token
                        && matches!(
                            stored_source.as_str(),
                            IDENTITY_SOURCE_LEGACY | IDENTITY_SOURCE_STATIC
                        ) =>
                {
                    let user_id = Uuid::parse_str(stored_id)?;
                    if user_id.is_nil() {
                        return Err(AppError::Conflict(
                            "configured static identity conflicts with an existing user"
                                .to_string(),
                        ));
                    }
                    actions.push(SeedAction::Existing { user_id, token });
                }
                _ => {
                    return Err(AppError::Conflict(
                        "configured static identity conflicts with an existing user".to_string(),
                    ));
                }
            }
        }

        for action in &actions {
            if let SeedAction::Insert {
                user_id,
                local_id,
                username,
                token,
                created_at,
            } = action
            {
                transaction.execute(
                    "INSERT INTO users \
                     (id, github_id, github_username, token, identity_source, created_at) \
                     VALUES (?, ?, ?, ?, 'static', ?)",
                    params![user_id.to_string(), local_id, username, token, created_at],
                )?;
            }
        }
        for action in &actions {
            if let SeedAction::Existing { user_id, .. } = action {
                transaction.execute(
                    "UPDATE users SET identity_source = 'static' WHERE id = ?",
                    params![user_id.to_string()],
                )?;
            }
        }
        transaction.commit()?;

        // Cache mutation happens only after the all-or-nothing DB commit.
        for action in actions {
            let (user_id, token) = match action {
                SeedAction::Existing { user_id, token }
                | SeedAction::Insert { user_id, token, .. } => (user_id, token),
            };
            self.token_cache.insert(token, user_id);
            info!("Static token seeded");
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Local-auth: explicitly enabled loopback registration
    // -----------------------------------------------------------------------

    /// Register a new local user (no GitHub required). Existing usernames are
    /// rejected: an unauthenticated enrollment path must never retrieve an
    /// existing bearer credential.
    pub async fn register_local_user(
        &self,
        username: &str,
        namespace: Option<&str>,
    ) -> Result<RegisterResponse> {
        if !self.config.local_registration_enabled {
            return Err(AppError::NotFound("local registration".to_string()));
        }
        if username.is_empty() || username.len() > 64 {
            return Err(AppError::InvalidInput(
                "username must be 1–64 characters".to_string(),
            ));
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(AppError::InvalidInput(
                "username may only contain alphanumeric characters, hyphens, and underscores"
                    .to_string(),
            ));
        }

        let desired_namespace = namespace
            .map(|n| n.trim_matches('/').to_string())
            .filter(|n| !n.is_empty());
        if desired_namespace.as_ref().is_some_and(|namespace| {
            namespace.len() > 64
                || !namespace.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
        }) {
            return Err(AppError::InvalidInput(
                "namespace must be 1–64 alphanumeric, hyphen, underscore, or dot characters"
                    .to_string(),
            ));
        }

        let local_id = username_to_local_id(username) as i64;
        let user_id = Uuid::new_v4();
        let token = self.generate_token();
        let created_at = Utc::now().to_rfc3339();
        {
            let conn = self.db.lock().unwrap();
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM users \
                 WHERE github_id = ? OR github_username = ? COLLATE NOCASE)",
                params![local_id, username],
                |row| row.get(0),
            )?;
            if exists {
                return Err(AppError::Conflict(
                    "local registration is unavailable for that username".to_string(),
                ));
            }
            conn.execute(
                "INSERT INTO users \
                 (id, github_id, github_username, token, namespace, identity_source, created_at) \
                 VALUES (?, ?, ?, ?, ?, 'local', ?)",
                params![
                    user_id.to_string(),
                    local_id,
                    username,
                    token,
                    desired_namespace,
                    created_at
                ],
            )?;
        }
        self.token_cache.insert(token.clone(), user_id);

        Ok(RegisterResponse {
            token,
            user_id,
            username: username.to_string(),
            namespace: desired_namespace,
        })
    }

    // -----------------------------------------------------------------------
    // GitHub OAuth
    // -----------------------------------------------------------------------

    fn random_urlsafe_secret() -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn issue_oauth_state(&self) -> Result<String> {
        let now = Instant::now();
        let mut states = self
            .oauth_states
            .lock()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("OAuth state lock poisoned")))?;
        states.retain(|_, expires_at| *expires_at > now);
        if states.len() >= MAX_PENDING_OAUTH_FLOWS {
            return Err(AppError::CapacityExceeded {
                details: "Too many pending OAuth flows".to_string(),
                retry_after_seconds: 1,
            });
        }

        let state = Self::random_urlsafe_secret();
        states.insert(state.clone(), now + OAUTH_STATE_TTL);
        Ok(state)
    }

    fn consume_oauth_state(&self, query_state: &str, cookie_state: Option<&str>) -> Result<()> {
        if cookie_state != Some(query_state) {
            return Err(AppError::InvalidInput(
                "invalid or expired OAuth state".to_string(),
            ));
        }

        let now = Instant::now();
        let mut states = self
            .oauth_states
            .lock()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("OAuth state lock poisoned")))?;
        let valid = states
            .remove(query_state)
            .is_some_and(|expires_at| expires_at > now);
        if !valid {
            return Err(AppError::InvalidInput(
                "invalid or expired OAuth state".to_string(),
            ));
        }

        Ok(())
    }

    fn issue_oauth_handoff(&self, response: AuthTokenResponse) -> Result<String> {
        let now = Instant::now();
        let mut handoffs = self
            .oauth_handoffs
            .lock()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("OAuth handoff lock poisoned")))?;
        handoffs.retain(|_, handoff| handoff.expires_at > now);
        if handoffs.len() >= MAX_PENDING_OAUTH_FLOWS {
            return Err(AppError::CapacityExceeded {
                details: "Too many pending OAuth handoffs".to_string(),
                retry_after_seconds: 1,
            });
        }

        let exchange_code = Self::random_urlsafe_secret();
        handoffs.insert(
            exchange_code.clone(),
            OAuthHandoff {
                response,
                expires_at: now + OAUTH_HANDOFF_TTL,
            },
        );
        Ok(exchange_code)
    }

    pub fn consume_oauth_handoff(&self, exchange_code: &str) -> Result<AuthTokenResponse> {
        let now = Instant::now();
        let mut handoffs = self
            .oauth_handoffs
            .lock()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("OAuth handoff lock poisoned")))?;
        let handoff = handoffs
            .remove(exchange_code)
            .filter(|handoff| handoff.expires_at > now);
        handoff.map(|handoff| handoff.response).ok_or_else(|| {
            AppError::InvalidInput("invalid or expired OAuth exchange code".to_string())
        })
    }

    pub fn github_auth_url(&self) -> Result<(String, String)> {
        let client_id = self.config.github_client_id.as_deref().unwrap_or_default();
        let state = self.issue_oauth_state()?;
        let url = format!(
            "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=user:email&state={}",
            client_id,
            urlencoding::encode(&self.config.github_redirect_uri),
            urlencoding::encode(&state)
        );
        Ok((url, state))
    }

    pub async fn handle_github_callback(
        &self,
        query: Query<GitHubCallbackQuery>,
        cookie_state: Option<&str>,
    ) -> Result<String> {
        self.consume_oauth_state(&query.state, cookie_state)?;
        let code = &query.code;
        let token_response = self.exchange_code_for_token(code).await?;
        let github_user = self.get_github_user(&token_response.access_token).await?;
        let user = self.create_or_get_user(github_user).await?;
        self.issue_oauth_handoff(AuthTokenResponse {
            token: user.token,
            user_id: user.id,
            github_username: user.github_username,
            namespace: user.namespace,
        })
    }

    async fn exchange_code_for_token(&self, code: &str) -> Result<GitHubTokenResponse> {
        let client_id = self.config.github_client_id.as_deref().unwrap_or_default();
        let client_secret = self
            .config
            .github_client_secret
            .as_deref()
            .unwrap_or_default();

        let mut params = HashMap::new();
        params.insert("client_id", client_id);
        params.insert("client_secret", client_secret);
        params.insert("code", code);

        #[cfg(test)]
        let token_endpoint = self.github_token_endpoint.as_str();
        #[cfg(not(test))]
        let token_endpoint = "https://github.com/login/oauth/access_token";

        let response = self
            .http_client
            .post(token_endpoint)
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AppError::UpstreamUnavailable {
                service: "GitHub OAuth token exchange",
            });
        }

        let token_response: GitHubTokenResponse = response.json().await?;
        Ok(token_response)
    }

    async fn get_github_user(&self, access_token: &str) -> Result<GitHubUser> {
        #[cfg(test)]
        let user_endpoint = self.github_user_endpoint.as_str();
        #[cfg(not(test))]
        let user_endpoint = "https://api.github.com/user";

        let response = self
            .http_client
            .get(user_endpoint)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", "octostore-lock")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AppError::UpstreamUnavailable {
                service: "GitHub user API",
            });
        }

        let github_user: GitHubUser = response.json().await?;
        Ok(github_user)
    }

    pub async fn create_or_get_user(&self, github_user: GitHubUser) -> Result<User> {
        let conn = self.db.lock().unwrap();
        let github_id = i64::try_from(github_user.id).map_err(|_| {
            AppError::InvalidInput("GitHub user id is outside the supported range".to_string())
        })?;

        let existing_user: Option<(User, String)> = conn
            .query_row(
                "SELECT id, github_id, github_username, token, namespace, created_at, identity_source \
                 FROM users WHERE github_id = ?",
                params![github_id],
                |row| {
                    Ok((
                        User {
                            id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap(),
                            github_id: row.get::<_, i64>(1)? as u64,
                            github_username: row.get(2)?,
                            token: row.get(3)?,
                            namespace: row.get(4)?,
                            created_at: chrono::DateTime::parse_from_rfc3339(
                                &row.get::<_, String>(5)?,
                            )
                            .unwrap()
                            .with_timezone(&chrono::Utc),
                        },
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;

        if let Some((mut user, identity_source)) = existing_user {
            if identity_source != IDENTITY_SOURCE_OAUTH {
                return Err(AppError::Conflict(
                    "GitHub identity conflicts with an existing non-OAuth identity".to_string(),
                ));
            }

            if user.github_username != github_user.login {
                let conflicting_user: Option<String> = conn
                    .query_row(
                        "SELECT id FROM users WHERE github_username = ? COLLATE NOCASE AND id <> ?",
                        params![github_user.login, user.id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if conflicting_user.is_some() {
                    return Err(AppError::Conflict(
                        "GitHub username conflicts with an existing identity".to_string(),
                    ));
                }
                conn.execute(
                    "UPDATE users SET github_username = ? WHERE id = ? AND identity_source = 'oauth'",
                    params![github_user.login, user.id.to_string()],
                )?;
                user.github_username = github_user.login;
            }
            debug!("Existing OAuth user logged in: {}", user.github_username);
            return Ok(user);
        }

        let same_name: Option<StoredIdentity> = conn
            .query_row(
                "SELECT id, github_id, github_username, token, namespace, created_at, identity_source FROM users \
                 WHERE github_username = ? COLLATE NOCASE",
                params![github_user.login],
                |row| {
                    Ok(StoredIdentity {
                        id: row.get(0)?,
                        github_id: row.get(1)?,
                        username: row.get(2)?,
                        token: row.get(3)?,
                        namespace: row.get(4)?,
                        created_at: row.get(5)?,
                        identity_source: row.get(6)?,
                    })
                },
            )
            .optional()?;
        if let Some(stored) = same_name {
            let is_bootstrap_identity = matches!(
                stored.identity_source.as_str(),
                IDENTITY_SOURCE_LEGACY | IDENTITY_SOURCE_LOCAL | IDENTITY_SOURCE_STATIC
            ) && stored.github_id
                == username_to_local_id(&stored.username) as i64;
            if !is_bootstrap_identity {
                return Err(AppError::Conflict(
                    "GitHub username conflicts with an existing identity".to_string(),
                ));
            }

            let user_id = Uuid::parse_str(&stored.id)?;
            let token = self.generate_token();
            conn.execute(
                "UPDATE users SET github_id = ?, github_username = ?, token = ?, \
                 identity_source = 'oauth' WHERE id = ?",
                params![github_id, github_user.login, token, user_id.to_string()],
            )?;
            self.token_cache.remove(&stored.token);
            self.token_cache.insert(token.clone(), user_id);
            return Ok(User {
                id: user_id,
                github_id: github_user.id,
                github_username: github_user.login,
                token,
                namespace: stored.namespace,
                created_at: chrono::DateTime::parse_from_rfc3339(&stored.created_at)
                    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?
                    .with_timezone(&Utc),
            });
        }

        let user_id = Uuid::new_v4();
        let token = self.generate_token();
        let created_at = Utc::now();

        conn.execute(
            "INSERT INTO users \
             (id, github_id, github_username, token, namespace, identity_source, created_at) \
             VALUES (?, ?, ?, ?, ?, 'oauth', ?)",
            params![
                user_id.to_string(),
                github_id,
                github_user.login,
                token,
                Option::<String>::None,
                created_at.to_rfc3339()
            ],
        )?;

        let user = User {
            id: user_id,
            github_id: github_user.id,
            github_username: github_user.login,
            token,
            namespace: None,
            created_at,
        };

        self.token_cache.insert(user.token.clone(), user.id);
        info!("New user created: {}", user.github_username);
        Ok(user)
    }

    // -----------------------------------------------------------------------
    // Token lifecycle
    // -----------------------------------------------------------------------

    pub async fn rotate_token(&self, current_token: &str) -> Result<String> {
        let conn = self.db.lock().unwrap();
        let new_token = self.generate_token();
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, identity_source FROM users WHERE token = ? AND id <> ?",
                params![current_token, Uuid::nil().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((user_id, identity_source)) = existing else {
            return Err(AppError::Unauthorized);
        };
        if !Self::identity_source_is_active(&self.config, &identity_source) {
            return Err(AppError::Unauthorized);
        }
        let user_id = Uuid::parse_str(&user_id)?;

        let updated_rows = conn.execute(
            "UPDATE users SET token = ? WHERE token = ? AND id = ?",
            params![new_token, current_token, user_id.to_string()],
        )?;

        if updated_rows == 0 {
            return Err(AppError::Unauthorized);
        }

        self.token_cache.remove(current_token);
        self.token_cache.insert(new_token.clone(), user_id);

        Ok(new_token)
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Uuid> {
        let bearer_token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));

        if let Some(admin_key) = &self.config.admin_key {
            let provided_key = headers
                .get("x-admin-key")
                .or_else(|| headers.get("x-octostore-admin-key"))
                .and_then(|v| v.to_str().ok());

            if provided_key == Some(admin_key) {
                return Ok(Uuid::nil());
            }
        }

        let token = bearer_token.ok_or(AppError::MissingAuth)?;

        if let Some(user_id) = self.token_cache.get(token).map(|value| *value) {
            if !user_id.is_nil() {
                return Ok(user_id);
            }
            self.token_cache.remove(token);
        }

        let conn = self.db.lock().unwrap();
        let (user_id, identity_source): (String, String) = conn
            .query_row(
                "SELECT id, identity_source FROM users WHERE token = ? AND id <> ?",
                params![token, Uuid::nil().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(AppError::Unauthorized)?;
        if !Self::identity_source_is_active(&self.config, &identity_source) {
            return Err(AppError::Unauthorized);
        }

        let uuid = Uuid::parse_str(&user_id)?;
        if uuid.is_nil() {
            return Err(AppError::Unauthorized);
        }
        self.token_cache.insert(token.to_string(), uuid);
        Ok(uuid)
    }
    fn generate_token(&self) -> String {
        let mut rng = rand::thread_rng();
        let token_bytes: [u8; 32] = rng.gen();
        base64::engine::general_purpose::STANDARD.encode(token_bytes)
    }

    // -----------------------------------------------------------------------
    // Fencing counter / user helpers (unchanged)
    // -----------------------------------------------------------------------

    pub fn load_fencing_counter(&self) -> Result<u64> {
        let conn = self.db.lock().unwrap();
        let counter: u64 = conn.query_row(
            "SELECT counter FROM fencing_counter WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(counter)
    }

    pub fn save_fencing_counter(&self, counter: u64) -> Result<()> {
        let conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE fencing_counter SET counter = MAX(counter, ?) WHERE id = 1",
            params![counter],
        )?;
        Ok(())
    }

    pub fn get_user_namespace(&self, user_id: Uuid) -> Result<Option<String>> {
        let conn = self.db.lock().unwrap();
        let namespace = conn
            .query_row(
                "SELECT namespace FROM users WHERE id = ?",
                params![user_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(namespace)
    }

    pub fn get_user_by_id(&self, user_id: &str) -> Result<Option<String>> {
        // Special case for admin nil UUID
        if user_id == Uuid::nil().to_string() {
            return Ok(Some("admin".to_string()));
        }

        let conn = self.db.lock().unwrap();
        let username: Option<String> = conn
            .query_row(
                "SELECT github_username FROM users WHERE id = ?",
                params![user_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(username)
    }

    pub fn is_oauth_user(&self, user_id: Uuid, expected_username: &str) -> Result<bool> {
        if user_id.is_nil() {
            return Ok(false);
        }
        let conn = self.db.lock().unwrap();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ? \
             AND github_username = ? COLLATE NOCASE AND identity_source = 'oauth')",
            params![user_id.to_string(), expected_username],
            |row| row.get(0),
        )
        .map_err(AppError::from)
    }

    pub fn get_all_users(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.db.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, github_username, created_at FROM users WHERE id <> ?")?;
        let user_rows = stmt.query_map(params![Uuid::nil().to_string()], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "github_username": row.get::<_, String>(1)?,
                "created_at": row.get::<_, String>(2)?
            }))
        })?;

        let mut users = Vec::new();
        for user_result in user_rows {
            users.push(user_result?);
        }
        Ok(users)
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

fn oauth_state_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find_map(|(name, value)| (name == OAUTH_STATE_COOKIE).then(|| value.to_string()))
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn dashboard_redirect(config: &Config, exchange_code: &str) -> Result<Redirect> {
    let dashboard = config.oauth_dashboard_url.as_deref().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "GitHub OAuth dashboard URL missing after startup validation"
        ))
    })?;
    let issuer = config.oauth_api_base_url.as_deref().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "GitHub OAuth API issuer missing after startup validation"
        ))
    })?;
    Ok(Redirect::to(&format!(
        "{dashboard}#exchange_code={}&issuer={}",
        urlencoding::encode(exchange_code),
        urlencoding::encode(issuer)
    )))
}

fn oauth_state_cookie(oauth_state: &str) -> String {
    format!(
        "{OAUTH_STATE_COOKIE}={oauth_state}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
        OAUTH_STATE_TTL.as_secs()
    )
}

pub async fn github_auth(State(state): State<crate::AppState>) -> Result<Response> {
    let (url, oauth_state) = state.auth_service.github_auth_url()?;
    let cookie = oauth_state_cookie(&oauth_state);
    let mut response = no_store(Redirect::to(&url).into_response());
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|error| AppError::Internal(anyhow::Error::new(error)))?,
    );
    Ok(response)
}

pub async fn github_callback(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Query<GitHubCallbackQuery>,
) -> Result<Response> {
    let cookie_state = oauth_state_from_headers(&headers);
    let exchange_code = state
        .auth_service
        .handle_github_callback(query, cookie_state.as_deref())
        .await?;
    let mut response = no_store(dashboard_redirect(&state.config, &exchange_code)?.into_response());
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-octostore-oauth-state=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax",
        ),
    );
    Ok(response)
}

pub async fn github_exchange(
    State(state): State<crate::AppState>,
    Json(payload): Json<OAuthExchangeRequest>,
) -> Result<Response> {
    let response = state
        .auth_service
        .consume_oauth_handoff(&payload.exchange_code)?;
    Ok(no_store(Json(response).into_response()))
}

pub async fn rotate_token(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Response> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let current_token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or(AppError::MissingAuth)?;

    let new_token = state.auth_service.rotate_token(current_token).await?;

    let conn = state.auth_service.db.lock().unwrap();
    let (github_username, namespace): (String, Option<String>) = conn.query_row(
        "SELECT github_username, namespace FROM users WHERE id = ?",
        params![user_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    Ok(no_store(
        Json(AuthTokenResponse {
            token: new_token,
            user_id,
            github_username,
            namespace,
        })
        .into_response(),
    ))
}

/// `POST /auth/register` — explicit loopback enrollment mode only.
/// Body: `{"username": "alice"}`
/// Returns a bearer token that works with all lock endpoints.
pub async fn register_local(
    State(state): State<crate::AppState>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Response> {
    if !state.config.local_registration_enabled {
        return Err(AppError::NotFound("local registration".to_string()));
    }
    let resp = state
        .auth_service
        .register_local_user(&payload.username, payload.namespace.as_deref())
        .await?;
    Ok(no_store(Json(resp).into_response()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use rusqlite::Connection;
    use tempfile::NamedTempFile;

    fn local_config() -> Config {
        Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: ":memory:".to_string(),
            github_client_id: None,
            github_client_secret: None,
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            oauth_api_base_url: None,
            oauth_dashboard_url: None,
            admin_key: Some("test_admin_key".to_string()),
            admin_username: None,
            static_tokens: None,
            static_tokens_file: None,
            local_registration_enabled: true,
            public_elections_enabled: true,
            max_public_elections: 100,
            public_election_requests_per_minute: 600,
            public_election_watch_streams_global: 100,
            public_election_watch_streams_per_client: 8,
            public_election_watch_max_seconds: 900,
        }
    }

    fn github_config() -> Config {
        Config {
            github_client_id: Some("test_client_id".to_string()),
            github_client_secret: Some("test_client_secret".to_string()),
            github_redirect_uri: "http://localhost:3000/auth/github/callback".to_string(),
            oauth_api_base_url: Some("http://localhost:3000".to_string()),
            oauth_dashboard_url: Some("http://localhost:4173/dashboard.html".to_string()),
            local_registration_enabled: false,
            ..local_config()
        }
    }

    fn make_service(config: Config) -> AuthService {
        let conn = Connection::open(&config.database_url).unwrap();
        let db: crate::store::DbConn = std::sync::Arc::new(std::sync::Mutex::new(conn));
        AuthService::new(config, db).unwrap()
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        HeaderMap::from_iter([(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        )])
    }

    // -- parse_token_list ---------------------------------------------------

    #[test]
    fn test_parse_token_list_user_token() {
        let pairs = parse_static_token_list("alice:tok1,bob:tok2", "test").unwrap();
        assert_eq!(
            pairs,
            vec![
                ("alice".to_string(), "tok1".to_string()),
                ("bob".to_string(), "tok2".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_token_list_bare() {
        let pairs = parse_static_token_list("mytoken", "test").unwrap();
        assert_eq!(pairs, vec![("mytoken".to_string(), "mytoken".to_string())]);
    }

    #[test]
    fn test_parse_token_list_file_format() {
        let raw = "# comment\nalice:tok1\nbob:tok2\n\n";
        let pairs = parse_static_token_list(raw, "test").unwrap();
        assert_eq!(pairs.len(), 2);
    }

    // -- username_to_local_id -----------------------------------------------

    #[test]
    fn test_local_id_stable() {
        assert_eq!(username_to_local_id("alice"), username_to_local_id("alice"));
    }

    #[test]
    fn test_local_id_different_users() {
        assert_ne!(username_to_local_id("alice"), username_to_local_id("bob"));
    }

    #[test]
    fn test_local_id_high_bit_set() {
        assert!(username_to_local_id("alice") >= (1u64 << 63));
    }

    // -- register_local_user -----------------------------------------------

    #[test]
    fn test_register_local_user() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt.block_on(svc.register_local_user("alice", None)).unwrap();
        assert_eq!(resp.username, "alice");
        assert!(!resp.token.is_empty());
    }

    #[test]
    fn test_register_local_user_rejects_repeated_registration_without_returning_a_token() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r1 = rt.block_on(svc.register_local_user("alice", None)).unwrap();
        assert!(!r1.token.is_empty());
        assert!(matches!(
            rt.block_on(svc.register_local_user("alice", None)),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    fn test_register_local_user_rejects_mixed_case_and_legacy_oauth_collisions() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(svc.register_local_user("alice", None)).unwrap();
        assert!(matches!(
            rt.block_on(svc.register_local_user("ALICE", None)),
            Err(AppError::Conflict(_))
        ));

        rt.block_on(svc.create_or_get_user(GitHubUser {
            id: 1234,
            login: "LegacyUser".to_string(),
        }))
        .unwrap();
        assert!(matches!(
            rt.block_on(svc.register_local_user("legacyuser", None)),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    fn test_auth_service_rejects_legacy_case_ambiguous_identities_at_startup() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (
                    id TEXT PRIMARY KEY,
                    github_id INTEGER NOT NULL UNIQUE,
                    github_username TEXT NOT NULL,
                    token TEXT NOT NULL UNIQUE,
                    created_at TEXT NOT NULL
                );",
            )
            .unwrap();
        for (github_id, username, token) in
            [(1_i64, "alice", "token-one"), (2_i64, "ALICE", "token-two")]
        {
            connection
                .execute(
                    "INSERT INTO users (id, github_id, github_username, token, created_at) \
                     VALUES (?, ?, ?, ?, ?)",
                    params![
                        Uuid::new_v4().to_string(),
                        github_id,
                        username,
                        token,
                        Utc::now().to_rfc3339()
                    ],
                )
                .unwrap();
        }
        let db = std::sync::Arc::new(std::sync::Mutex::new(connection));
        assert!(AuthService::new(local_config(), db).is_err());
    }

    #[test]
    fn test_register_local_user_rejects_a_static_user_collision() {
        let mut config = local_config();
        config.static_tokens = Some("ops:static-secret".to_string());
        let svc = make_service(config);
        svc.seed_static_tokens().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(matches!(
            rt.block_on(svc.register_local_user("ops", None)),
            Err(AppError::Conflict(_))
        ));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer static-secret".parse().unwrap());
        assert!(svc.authenticate(&headers).is_ok());
    }

    #[test]
    fn test_register_local_user_rejects_empty() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(svc.register_local_user("", None)).is_err());
    }

    #[test]
    fn test_register_local_user_rejects_bad_chars() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt
            .block_on(svc.register_local_user("alice@example.com", None))
            .is_err());
    }

    #[test]
    fn test_register_local_user_rejects_bad_namespace_as_invalid_input() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(matches!(
            rt.block_on(svc.register_local_user("alice", Some("team one"))),
            Err(AppError::InvalidInput(_))
        ));
    }

    // -- static token seeding ----------------------------------------------

    #[test]
    fn test_seed_static_tokens() {
        let mut cfg = local_config();
        cfg.static_tokens = Some("alice:mytoken123".to_string());
        let svc = make_service(cfg);
        svc.seed_static_tokens().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer mytoken123".parse().unwrap());
        let user_id = svc.authenticate(&headers).unwrap();
        assert_eq!(svc.get_user_namespace(user_id).unwrap(), None);
    }

    #[test]
    fn admin_key_is_not_accepted_as_a_bearer_token() {
        let svc = make_service(local_config());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test_admin_key".parse().unwrap());
        assert!(matches!(
            svc.authenticate(&headers),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn test_seed_static_tokens_from_file() {
        let mut file = NamedTempFile::new().unwrap();
        use std::io::Write;
        writeln!(file, "# comment").unwrap();
        writeln!(file, "fileuser:filetoken456").unwrap();

        let mut cfg = local_config();
        cfg.static_tokens_file = Some(file.path().to_str().unwrap().to_string());
        let svc = make_service(cfg);
        svc.seed_static_tokens().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer filetoken456".parse().unwrap());
        assert!(svc.authenticate(&headers).is_ok());
    }

    #[test]
    fn static_token_seeding_is_all_or_nothing_across_late_conflicts_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = directory
            .path()
            .join("static-seeding.db")
            .to_string_lossy()
            .into_owned();

        let mut initial = local_config();
        initial.database_url = database_url.clone();
        initial.local_registration_enabled = false;
        initial.static_tokens = Some("existing-user:correct-token".to_string());
        let initial_service = make_service(initial);
        initial_service.seed_static_tokens().unwrap();
        drop(initial_service);

        let mut conflicting = local_config();
        conflicting.database_url = database_url.clone();
        conflicting.local_registration_enabled = false;
        conflicting.static_tokens =
            Some("new-user:new-token,existing-user:wrong-token".to_string());
        let conflicting_service = make_service(conflicting);
        assert!(matches!(
            conflicting_service.seed_static_tokens(),
            Err(AppError::Conflict(_))
        ));
        let mut new_headers = HeaderMap::new();
        new_headers.insert("authorization", "Bearer new-token".parse().unwrap());
        assert!(matches!(
            conflicting_service.authenticate(&new_headers),
            Err(AppError::Unauthorized)
        ));
        let new_rows: i64 = conflicting_service
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM users WHERE github_username = 'new-user'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_rows, 0);
        drop(conflicting_service);

        let mut restarted = local_config();
        restarted.database_url = database_url;
        restarted.local_registration_enabled = false;
        let restarted_service = make_service(restarted);
        assert!(matches!(
            restarted_service.authenticate(&new_headers),
            Err(AppError::Unauthorized)
        ));
        let mut existing_headers = HeaderMap::new();
        existing_headers.insert("authorization", "Bearer correct-token".parse().unwrap());
        assert!(restarted_service.authenticate(&existing_headers).is_ok());
    }

    #[test]
    fn static_token_seeding_rejects_legacy_hash_and_multirow_matches() {
        for multiple_matches in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let database_url = directory
                .path()
                .join("legacy-static.db")
                .to_string_lossy()
                .into_owned();
            let mut base = local_config();
            base.database_url = database_url.clone();
            base.local_registration_enabled = false;
            let base_service = make_service(base);
            {
                let conn = base_service.db.lock().unwrap();
                conn.execute(
                    "INSERT INTO users (id, github_id, github_username, token, created_at) \
                     VALUES (?, ?, ?, ?, ?)",
                    params![
                        Uuid::new_v4().to_string(),
                        username_to_local_id("target-user") as i64,
                        "different-user",
                        if multiple_matches {
                            "different-token"
                        } else {
                            "target-token"
                        },
                        Utc::now().to_rfc3339()
                    ],
                )
                .unwrap();
                if multiple_matches {
                    conn.execute(
                        "INSERT INTO users (id, github_id, github_username, token, created_at) \
                         VALUES (?, ?, ?, ?, ?)",
                        params![
                            Uuid::new_v4().to_string(),
                            1234_i64,
                            "target-user",
                            "target-token",
                            Utc::now().to_rfc3339()
                        ],
                    )
                    .unwrap();
                }
            }
            drop(base_service);

            let mut configured = local_config();
            configured.database_url = database_url;
            configured.local_registration_enabled = false;
            configured.static_tokens = Some("target-user:target-token".to_string());
            let configured_service = make_service(configured);
            assert!(matches!(
                configured_service.seed_static_tokens(),
                Err(AppError::Conflict(_))
            ));
        }
    }

    #[test]
    fn checked_static_token_seeding_rejects_an_unreadable_configured_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = local_config();
        cfg.static_tokens_file = Some(
            directory
                .path()
                .join("missing.tokens")
                .to_string_lossy()
                .into_owned(),
        );
        let svc = make_service(cfg);
        assert!(matches!(
            svc.seed_static_tokens(),
            Err(AppError::Internal(_))
        ));
    }

    #[test]
    fn legacy_migration_and_mode_switches_activate_only_proven_identity_sources() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (
                    id TEXT PRIMARY KEY,
                    github_id INTEGER NOT NULL UNIQUE,
                    github_username TEXT NOT NULL,
                    token TEXT NOT NULL UNIQUE,
                    created_at TEXT NOT NULL
                );",
            )
            .unwrap();
        let oauth_user_id = Uuid::new_v4();
        let local_user_id = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO users (id, github_id, github_username, token, created_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    oauth_user_id.to_string(),
                    42_i64,
                    "octoadmin",
                    "legacy-oauth-token",
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO users (id, github_id, github_username, token, created_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    local_user_id.to_string(),
                    username_to_local_id("local-user") as i64,
                    "local-user",
                    "legacy-local-token",
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        let db: crate::store::DbConn = Arc::new(Mutex::new(connection));

        let oauth_service = AuthService::new(github_config(), db.clone()).unwrap();
        assert_eq!(
            oauth_service
                .authenticate(&bearer_headers("legacy-oauth-token"))
                .unwrap(),
            oauth_user_id
        );
        assert!(oauth_service
            .is_oauth_user(oauth_user_id, "OCTOADMIN")
            .unwrap());
        assert!(matches!(
            oauth_service.authenticate(&bearer_headers("legacy-local-token")),
            Err(AppError::Unauthorized)
        ));
        drop(oauth_service);

        let mut local = local_config();
        local.local_registration_enabled = false;
        let local_service = AuthService::new(local, db).unwrap();
        assert_eq!(
            local_service
                .authenticate(&bearer_headers("legacy-local-token"))
                .unwrap(),
            local_user_id
        );
        assert!(matches!(
            local_service.authenticate(&bearer_headers("legacy-oauth-token")),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn oauth_login_promotes_only_a_proven_bootstrap_identity_and_rotates_its_token() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = directory
            .path()
            .join("identity-promotion.db")
            .to_string_lossy()
            .into_owned();

        let mut bootstrap = local_config();
        bootstrap.database_url = database_url.clone();
        bootstrap.local_registration_enabled = false;
        bootstrap.static_tokens = Some("octoadmin:stale-static-token".to_string());
        let bootstrap_service = make_service(bootstrap);
        bootstrap_service.seed_static_tokens().unwrap();
        let bootstrap_user_id = bootstrap_service
            .authenticate(&bearer_headers("stale-static-token"))
            .unwrap();
        drop(bootstrap_service);

        let mut oauth = github_config();
        oauth.database_url = database_url.clone();
        oauth.admin_username = Some("octoadmin".to_string());
        let oauth_service = make_service(oauth);
        assert!(matches!(
            oauth_service.authenticate(&bearer_headers("stale-static-token")),
            Err(AppError::Unauthorized)
        ));
        assert!(!oauth_service
            .is_oauth_user(bootstrap_user_id, "octoadmin")
            .unwrap());

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let promoted = runtime
            .block_on(oauth_service.create_or_get_user(GitHubUser {
                id: 867_5309,
                login: "OctoAdmin".to_string(),
            }))
            .unwrap();
        assert_eq!(promoted.id, bootstrap_user_id);
        assert_ne!(promoted.token, "stale-static-token");
        assert_eq!(
            oauth_service
                .authenticate(&bearer_headers(&promoted.token))
                .unwrap(),
            bootstrap_user_id
        );
        assert!(oauth_service
            .is_oauth_user(bootstrap_user_id, "octoadmin")
            .unwrap());
        assert!(matches!(
            oauth_service.authenticate(&bearer_headers("stale-static-token")),
            Err(AppError::Unauthorized)
        ));
        drop(oauth_service);

        let mut local = local_config();
        local.database_url = database_url;
        local.local_registration_enabled = false;
        let local_service = make_service(local);
        assert!(matches!(
            local_service.authenticate(&bearer_headers(&promoted.token)),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn oauth_login_updates_the_exact_identity_but_rejects_same_name_id_conflicts() {
        let service = make_service(github_config());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let created = runtime
            .block_on(service.create_or_get_user(GitHubUser {
                id: 1001,
                login: "old-name".to_string(),
            }))
            .unwrap();
        let renamed = runtime
            .block_on(service.create_or_get_user(GitHubUser {
                id: 1001,
                login: "new-name".to_string(),
            }))
            .unwrap();
        assert_eq!(renamed.id, created.id);
        assert_eq!(renamed.token, created.token);
        assert_eq!(renamed.github_username, "new-name");

        assert!(matches!(
            runtime.block_on(service.create_or_get_user(GitHubUser {
                id: 2002,
                login: "NEW-NAME".to_string(),
            })),
            Err(AppError::Conflict(_))
        ));
        assert_eq!(
            service
                .authenticate(&bearer_headers(&created.token))
                .unwrap(),
            created.id
        );
    }

    #[test]
    fn oauth_login_rejects_ids_outside_sqlites_supported_range() {
        let service = make_service(github_config());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(matches!(
            runtime.block_on(service.create_or_get_user(GitHubUser {
                id: u64::MAX,
                login: "outside-range".to_string(),
            })),
            Err(AppError::InvalidInput(_))
        ));
    }

    // -- github_auth_url with optional creds --------------------------------

    #[test]
    fn test_github_auth_url_with_creds() {
        let svc = make_service(github_config());
        let (url, state) = svc.github_auth_url().unwrap();
        assert!(url.contains("client_id=test_client_id"));
        assert!(url.contains(&format!("state={state}")));
    }

    #[test]
    fn test_github_auth_url_without_creds() {
        let svc = make_service(local_config());
        let (url, _) = svc.github_auth_url().unwrap();
        assert!(url.contains("https://github.com/login/oauth/authorize"));
    }

    #[test]
    fn oauth_state_is_cookie_bound_single_use_and_rejects_replay() {
        let svc = make_service(github_config());
        let (_, state) = svc.github_auth_url().unwrap();

        assert!(matches!(
            svc.consume_oauth_state(&state, Some("different-state")),
            Err(AppError::InvalidInput(_))
        ));
        svc.consume_oauth_state(&state, Some(&state)).unwrap();
        assert!(matches!(
            svc.consume_oauth_state(&state, Some(&state)),
            Err(AppError::InvalidInput(_))
        ));
    }

    #[test]
    fn oauth_state_rejects_missing_cookie_and_expiry() {
        let svc = make_service(github_config());
        let (_, missing_cookie_state) = svc.github_auth_url().unwrap();
        assert!(matches!(
            svc.consume_oauth_state(&missing_cookie_state, None),
            Err(AppError::InvalidInput(_))
        ));

        let expired_state = "expired-state".to_string();
        svc.oauth_states.lock().unwrap().insert(
            expired_state.clone(),
            Instant::now() - Duration::from_secs(1),
        );
        assert!(matches!(
            svc.consume_oauth_state(&expired_state, Some(&expired_state)),
            Err(AppError::InvalidInput(_))
        ));
    }

    #[test]
    fn callback_location_contains_only_one_time_handoff_code() {
        let config = github_config();
        let svc = make_service(config.clone());
        let api_token = "api-bearer-secret+/=";
        let exchange_code = svc
            .issue_oauth_handoff(AuthTokenResponse {
                token: api_token.to_string(),
                user_id: Uuid::new_v4(),
                github_username: "sensitive-user".to_string(),
                namespace: Some("sensitive-namespace".to_string()),
            })
            .unwrap();

        let response = no_store(
            dashboard_redirect(&config, &exchange_code)
                .unwrap()
                .into_response(),
        );
        assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(location.starts_with("http://localhost:4173/dashboard.html#exchange_code="));
        assert!(location.contains("&issuer=http%3A%2F%2Flocalhost%3A3000"));
        let encoded_api_token = urlencoding::encode(api_token);
        for forbidden in [
            api_token,
            encoded_api_token.as_ref(),
            "token=",
            "username=",
            "user_id=",
            "sensitive-user",
        ] {
            assert!(!location.contains(forbidden), "Location leaked {forbidden}");
        }

        let handoff = svc.consume_oauth_handoff(&exchange_code).unwrap();
        assert_eq!(handoff.token, api_token);
        assert!(matches!(
            svc.consume_oauth_handoff(&exchange_code),
            Err(AppError::InvalidInput(_))
        ));

        svc.oauth_handoffs.lock().unwrap().insert(
            "expired-exchange".to_string(),
            OAuthHandoff {
                response: AuthTokenResponse {
                    token: "expired-token".to_string(),
                    user_id: Uuid::new_v4(),
                    github_username: "expired-user".to_string(),
                    namespace: None,
                },
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );
        assert!(matches!(
            svc.consume_oauth_handoff("expired-exchange"),
            Err(AppError::InvalidInput(_))
        ));
    }

    #[test]
    fn oauth_state_cookie_parser_finds_the_host_only_cookie() {
        let set_cookie = oauth_state_cookie("state-value");
        for required in [
            "__Host-octostore-oauth-state=state-value",
            "Path=/",
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
        ] {
            assert!(set_cookie.contains(required));
        }

        let headers = HeaderMap::from_iter([(
            header::COOKIE,
            HeaderValue::from_static("other=x; __Host-octostore-oauth-state=state-value"),
        )]);
        assert_eq!(
            oauth_state_from_headers(&headers).as_deref(),
            Some("state-value")
        );
    }

    // -- existing tests (unchanged) ----------------------------------------

    #[test]
    fn test_authenticate_valid_token() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt
            .block_on(svc.register_local_user("testuser", None))
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", resp.token).parse().unwrap(),
        );
        assert!(svc.authenticate(&headers).is_ok());
    }

    #[test]
    fn test_authenticate_invalid_token() {
        let svc = make_service(local_config());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer invalid".parse().unwrap());
        assert!(matches!(
            svc.authenticate(&headers),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn test_authenticate_missing_header() {
        let svc = make_service(local_config());
        assert!(matches!(
            svc.authenticate(&HeaderMap::new()),
            Err(AppError::MissingAuth)
        ));
    }

    #[test]
    fn synthetic_admin_never_becomes_a_bearer_user_before_or_after_restart() {
        let database = NamedTempFile::new().unwrap();
        let mut config = local_config();
        config.database_url = database.path().to_string_lossy().to_string();

        let service = make_service(config.clone());
        let admin_headers = HeaderMap::from_iter([(
            "x-admin-key".parse().unwrap(),
            HeaderValue::from_static("test_admin_key"),
        )]);
        assert_eq!(service.authenticate(&admin_headers).unwrap(), Uuid::nil());

        let legacy_bearer = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer admin-internal-token"),
        )]);
        assert!(matches!(
            service.authenticate(&legacy_bearer),
            Err(AppError::Unauthorized)
        ));
        assert_eq!(
            service
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM users WHERE id = ?",
                    params![Uuid::nil().to_string()],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            0
        );

        service
            .db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO users (id, github_id, github_username, token, created_at) VALUES (?, ?, ?, ?, ?)",
                params![
                    Uuid::nil().to_string(),
                    0,
                    "legacy-admin",
                    "admin-internal-token",
                    Utc::now().to_rfc3339(),
                ],
            )
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(matches!(
            runtime.block_on(service.rotate_token("admin-internal-token")),
            Err(AppError::Unauthorized)
        ));
        assert!(matches!(
            service.authenticate(&legacy_bearer),
            Err(AppError::Unauthorized)
        ));
        drop(service);

        let restarted = make_service(config);
        assert!(matches!(
            restarted.authenticate(&legacy_bearer),
            Err(AppError::Unauthorized)
        ));
        assert_eq!(
            restarted
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM users WHERE id = ?",
                    params![Uuid::nil().to_string()],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_fencing_counter() {
        let svc = make_service(local_config());
        assert_eq!(svc.load_fencing_counter().unwrap(), 0);
        svc.save_fencing_counter(42).unwrap();
        assert_eq!(svc.load_fencing_counter().unwrap(), 42);
    }

    #[test]
    fn test_auth_service_new_memory() {
        let cfg = local_config();
        let conn = Connection::open(&cfg.database_url).unwrap();
        let db: crate::store::DbConn = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let svc = AuthService::new(cfg, db);
        assert!(svc.is_ok());
    }

    #[test]
    fn test_parse_token_list_whitespace() {
        let pairs = parse_static_token_list(" alice : tok1 , bob : tok2 ", "test").unwrap();
        assert_eq!(
            pairs,
            vec![
                ("alice".to_string(), "tok1".to_string()),
                ("bob".to_string(), "tok2".to_string()),
            ]
        );
    }
}
