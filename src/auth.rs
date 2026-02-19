use crate::{
    config::Config,
    error::{AppError, Result},
    models::{AuthTokenResponse, GitHubTokenResponse, GitHubUser, User},
};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Redirect,
    Json,
};
use base64::Engine;
use chrono::Utc;
use rand::Rng;
use reqwest::Client;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap};
use dashmap::DashMap;
use tracing::{debug, info, warn};
use uuid::Uuid;

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

/// Parse `"alice:tok1,bob:tok2"` or `"tok1,tok2"` (bare token ⇒ username = token).
/// Also handles newline-delimited input from a file (# = comment).
fn parse_token_list(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .flat_map(|line| line.split(','))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .map(|entry| {
            if let Some((user, tok)) = entry.split_once(':') {
                (user.trim().to_string(), tok.trim().to_string())
            } else {
                // bare token — use itself as the username
                (entry.to_string(), entry.to_string())
            }
        })
        .collect()
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
    token_cache: std::sync::Arc<DashMap<String, Uuid>>,
}

#[derive(Deserialize)]
pub struct GitHubCallbackQuery {
    code: String,
    #[allow(dead_code)]
    state: Option<String>,
}

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub token: String,
    pub user_id: Uuid,
    pub username: String,
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
                created_at TEXT NOT NULL
            )
            "#,
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

            info!("Database initialized at: {}", config.database_url);
        }

        // Pre-load existing tokens into cache
        let token_cache = DashMap::new();
        {
            let conn = db.lock().unwrap();
            let mut stmt = conn.prepare("SELECT token, id FROM users")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                if let Ok((token, id)) = row {
                    if let Ok(uuid) = Uuid::parse_str(&id) {
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
            token_cache: std::sync::Arc::new(token_cache),
        })
    }

    // -----------------------------------------------------------------------
    // Local-auth: static token seeding (fully synchronous)
    // -----------------------------------------------------------------------

    /// Seed tokens from `STATIC_TOKENS` / `STATIC_TOKENS_FILE` into SQLite.
    /// Safe to call from both sync and async contexts — no Tokio required.
    pub fn seed_static_tokens(&self) {
        let mut pairs: Vec<(String, String)> = Vec::new();

        if let Some(raw) = &self.config.static_tokens {
            pairs.extend(parse_token_list(raw));
        }

        if let Some(path) = &self.config.static_tokens_file {
            match std::fs::read_to_string(path) {
                Ok(contents) => pairs.extend(parse_token_list(&contents)),
                Err(e) => warn!("Could not read STATIC_TOKENS_FILE {}: {}", path, e),
            }
        }

        for (username, token) in pairs {
            let local_id = username_to_local_id(&username) as i64;
            let user_id = Uuid::new_v4();
            let created_at = Utc::now().to_rfc3339();

            let conn = self.db.lock().unwrap();

            // Insert if absent (IGNORE = keep existing row)
            if let Err(e) = conn.execute(
                "INSERT OR IGNORE INTO users \
                 (id, github_id, github_username, token, created_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params![user_id.to_string(), local_id, username, token, created_at],
            ) {
                warn!("Failed to insert static user '{}': {}", username, e);
                continue;
            }

            // Load whatever is stored now (may be from this insert or a prior one)
            let stored: Option<(String, String)> = conn
                .query_row(
                    "SELECT id, token FROM users WHERE github_id = ?",
                    params![local_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();

            if let Some((stored_id, stored_token)) = stored {
                if let Ok(uid) = Uuid::parse_str(&stored_id) {
                    self.token_cache.insert(stored_token.clone(), uid);
                    if stored_token == token {
                        info!("Static token seeded for user '{}'", username);
                    } else {
                        info!("User '{}' already exists; keeping existing token", username);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Local-auth: open registration
    // -----------------------------------------------------------------------

    /// Register a new local user (no GitHub required).  Idempotent — returns
    /// the existing token if the username was already registered.
    pub async fn register_local_user(&self, username: &str) -> Result<RegisterResponse> {
        if username.is_empty() || username.len() > 64 {
            return Err(AppError::Internal(anyhow::anyhow!(
                "username must be 1–64 characters"
            )));
        }
        if !username.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return Err(AppError::Internal(anyhow::anyhow!(
                "username may only contain alphanumeric characters, hyphens, and underscores"
            )));
        }

        let local_id = username_to_local_id(username);
        let github_user = GitHubUser { id: local_id, login: username.to_string() };
        let user = self.create_or_get_user(github_user).await?;

        Ok(RegisterResponse {
            token: user.token,
            user_id: user.id,
            username: user.github_username,
        })
    }

    // -----------------------------------------------------------------------
    // GitHub OAuth
    // -----------------------------------------------------------------------

    pub fn github_auth_url(&self) -> String {
        let client_id = self.config.github_client_id.as_deref().unwrap_or_default();
        format!(
            "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=user:email",
            client_id,
            urlencoding::encode(&self.config.github_redirect_uri)
        )
    }

    pub async fn handle_github_callback(
        &self,
        query: Query<GitHubCallbackQuery>,
    ) -> Result<AuthTokenResponse> {
        let code = &query.code;
        let token_response = self.exchange_code_for_token(code).await?;
        let github_user = self.get_github_user(&token_response.access_token).await?;
        let user = self.create_or_get_user(github_user).await?;
        Ok(AuthTokenResponse {
            token: user.token,
            user_id: user.id,
            github_username: user.github_username,
        })
    }

    async fn exchange_code_for_token(&self, code: &str) -> Result<GitHubTokenResponse> {
        let client_id = self.config.github_client_id.as_deref().unwrap_or_default();
        let client_secret = self.config.github_client_secret.as_deref().unwrap_or_default();

        let mut params = HashMap::new();
        params.insert("client_id", client_id);
        params.insert("client_secret", client_secret);
        params.insert("code", code);

        let response = self
            .http_client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AppError::Internal(anyhow::anyhow!(
                "GitHub token exchange failed"
            )));
        }

        let token_response: GitHubTokenResponse = response.json().await?;
        Ok(token_response)
    }

    async fn get_github_user(&self, access_token: &str) -> Result<GitHubUser> {
        let response = self
            .http_client
            .get("https://api.github.com/user")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", "octostore-lock")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Failed to get GitHub user"
            )));
        }

        let github_user: GitHubUser = response.json().await?;
        Ok(github_user)
    }

    pub async fn create_or_get_user(&self, github_user: GitHubUser) -> Result<User> {
        let conn = self.db.lock().unwrap();

        let existing_user: Option<User> = conn
            .query_row(
                "SELECT id, github_id, github_username, token, created_at FROM users WHERE github_id = ?",
                params![github_user.id as i64],
                |row| {
                    Ok(User {
                        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap(),
                        github_id: row.get::<_, i64>(1)? as u64,
                        github_username: row.get(2)?,
                        token: row.get(3)?,
                        created_at: chrono::DateTime::parse_from_rfc3339(
                            &row.get::<_, String>(4)?,
                        )
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                    })
                },
            )
            .optional()?;

        if let Some(user) = existing_user {
            debug!("Existing user logged in: {}", user.github_username);
            return Ok(user);
        }

        let user_id = Uuid::new_v4();
        let token = self.generate_token();
        let created_at = Utc::now();

        conn.execute(
            "INSERT INTO users (id, github_id, github_username, token, created_at) VALUES (?, ?, ?, ?, ?)",
            params![
                user_id.to_string(),
                github_user.id as i64,
                github_user.login,
                token,
                created_at.to_rfc3339()
            ],
        )?;

        let user = User {
            id: user_id,
            github_id: github_user.id,
            github_username: github_user.login,
            token,
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
        let user_id = self.token_cache.get(current_token).map(|v| *v);
        let conn = self.db.lock().unwrap();
        let new_token = self.generate_token();

        let updated_rows = conn.execute(
            "UPDATE users SET token = ? WHERE token = ?",
            params![new_token, current_token],
        )?;

        if updated_rows == 0 {
            return Err(AppError::Unauthorized);
        }

        self.token_cache.remove(current_token);
        if let Some(uid) = user_id {
            self.token_cache.insert(new_token.clone(), uid);
        }

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
                .and_then(|v| v.to_str().ok())
                .or(bearer_token);
                
            if provided_key == Some(admin_key) {
                let admin_uuid = Uuid::nil();
                
                // Ensure admin exists in DB so foreign keys/lookups don't fail
                let conn = self.db.lock().unwrap();
                let exists: bool = conn.query_row(
                    "SELECT 1 FROM users WHERE id = ?",
                    params![admin_uuid.to_string()],
                    |_| Ok(true)
                ).unwrap_or(false);
                
                if !exists {
                    let _ = conn.execute(
                        "INSERT OR IGNORE INTO users (id, github_id, github_username, token, created_at) \
                         VALUES (?, ?, ?, ?, ?)",
                        params![
                            admin_uuid.to_string(),
                            0,
                            "admin",
                            "admin-internal-token",
                            Utc::now().to_rfc3339()
                        ],
                    );
                }
                
                return Ok(admin_uuid);
            }
        }

        let token = bearer_token.ok_or(AppError::MissingAuth)?;

        if let Some(user_id) = self.token_cache.get(token) {
            return Ok(*user_id);
        }

        let conn = self.db.lock().unwrap();
        let user_id: String = conn
            .query_row(
                "SELECT id FROM users WHERE token = ?",
                params![token],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(AppError::Unauthorized)?;

        let uuid = Uuid::parse_str(&user_id)?;
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
            "UPDATE fencing_counter SET counter = ? WHERE id = 1",
            params![counter],
        )?;
        Ok(())
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

    pub fn get_all_users(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.db.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, github_username, created_at FROM users")?;
        let user_rows = stmt.query_map([], |row| {
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

pub async fn github_auth(State(state): State<crate::AppState>) -> Redirect {
    let url = state.auth_service.github_auth_url();
    Redirect::permanent(&url)
}

pub async fn github_callback(
    State(state): State<crate::AppState>,
    query: Query<GitHubCallbackQuery>,
) -> Result<Redirect> {
    let response = state.auth_service.handle_github_callback(query).await?;
    let dashboard_url = format!(
        "https://octostore.io/dashboard.html?token={}&username={}&user_id={}",
        urlencoding::encode(&response.token),
        urlencoding::encode(&response.github_username),
        response.user_id
    );
    Ok(Redirect::permanent(&dashboard_url))
}

pub async fn rotate_token(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthTokenResponse>> {
    let user_id = state.auth_service.authenticate(&headers)?;

    let current_token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or(AppError::MissingAuth)?;

    let new_token = state.auth_service.rotate_token(current_token).await?;

    let conn = state.auth_service.db.lock().unwrap();
    let github_username: String = conn.query_row(
        "SELECT github_username FROM users WHERE id = ?",
        params![user_id.to_string()],
        |row| row.get(0),
    )?;

    Ok(Json(AuthTokenResponse {
        token: new_token,
        user_id,
        github_username,
    }))
}

/// `POST /auth/register` — local-auth mode only.
/// Body: `{"username": "alice"}`
/// Returns a bearer token that works with all lock endpoints.
pub async fn register_local(
    State(state): State<crate::AppState>,
    Json(payload): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>> {
    if state.config.is_github_enabled() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Local registration is disabled when GitHub OAuth is configured. \
             Use /auth/github to sign in."
        )));
    }
    let resp = state
        .auth_service
        .register_local_user(&payload.username)
        .await?;
    Ok(Json(resp))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use crate::config::Config;
    use tempfile::NamedTempFile;

    fn local_config() -> Config {
        Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: ":memory:".to_string(),
            github_client_id: None,
            github_client_secret: None,
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: Some("test_admin_key".to_string()),
            admin_username: None,
            static_tokens: None,
            static_tokens_file: None,
        }
    }

    fn github_config() -> Config {
        Config {
            github_client_id: Some("test_client_id".to_string()),
            github_client_secret: Some("test_client_secret".to_string()),
            ..local_config()
        }
    }

    fn make_service(config: Config) -> AuthService {
        let conn = Connection::open(&config.database_url).unwrap();
        let db: crate::store::DbConn = std::sync::Arc::new(std::sync::Mutex::new(conn));
        AuthService::new(config, db).unwrap()
    }

    // -- parse_token_list ---------------------------------------------------

    #[test]
    fn test_parse_token_list_user_token() {
        let pairs = parse_token_list("alice:tok1,bob:tok2");
        assert_eq!(pairs, vec![
            ("alice".to_string(), "tok1".to_string()),
            ("bob".to_string(), "tok2".to_string()),
        ]);
    }

    #[test]
    fn test_parse_token_list_bare() {
        let pairs = parse_token_list("mytoken");
        assert_eq!(pairs, vec![("mytoken".to_string(), "mytoken".to_string())]);
    }

    #[test]
    fn test_parse_token_list_file_format() {
        let raw = "# comment\nalice:tok1\nbob:tok2\n\n";
        let pairs = parse_token_list(raw);
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
        let resp = rt.block_on(svc.register_local_user("alice")).unwrap();
        assert_eq!(resp.username, "alice");
        assert!(!resp.token.is_empty());
    }

    #[test]
    fn test_register_local_user_idempotent() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r1 = rt.block_on(svc.register_local_user("alice")).unwrap();
        let r2 = rt.block_on(svc.register_local_user("alice")).unwrap();
        assert_eq!(r1.token, r2.token);
        assert_eq!(r1.user_id, r2.user_id);
    }

    #[test]
    fn test_register_local_user_rejects_empty() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(svc.register_local_user("")).is_err());
    }

    #[test]
    fn test_register_local_user_rejects_bad_chars() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(svc.register_local_user("alice@example.com")).is_err());
    }

    // -- static token seeding ----------------------------------------------

    #[test]
    fn test_seed_static_tokens() {
        let mut cfg = local_config();
        cfg.static_tokens = Some("alice:mytoken123".to_string());
        let svc = make_service(cfg);
        svc.seed_static_tokens();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer mytoken123".parse().unwrap());
        assert!(svc.authenticate(&headers).is_ok());
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
        svc.seed_static_tokens();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer filetoken456".parse().unwrap());
        assert!(svc.authenticate(&headers).is_ok());
    }

    // -- github_auth_url with optional creds --------------------------------

    #[test]
    fn test_github_auth_url_with_creds() {
        let svc = make_service(github_config());
        assert!(svc.github_auth_url().contains("client_id=test_client_id"));
    }

    #[test]
    fn test_github_auth_url_without_creds() {
        let svc = make_service(local_config());
        let url = svc.github_auth_url();
        assert!(url.contains("https://github.com/login/oauth/authorize"));
    }

    // -- existing tests (unchanged) ----------------------------------------

    #[test]
    fn test_authenticate_valid_token() {
        let svc = make_service(local_config());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt.block_on(svc.register_local_user("testuser")).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", resp.token).parse().unwrap());
        assert!(svc.authenticate(&headers).is_ok());
    }

    #[test]
    fn test_authenticate_invalid_token() {
        let svc = make_service(local_config());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer invalid".parse().unwrap());
        assert!(matches!(svc.authenticate(&headers), Err(AppError::Unauthorized)));
    }

    #[test]
    fn test_authenticate_missing_header() {
        let svc = make_service(local_config());
        assert!(matches!(svc.authenticate(&HeaderMap::new()), Err(AppError::MissingAuth)));
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
        let pairs = parse_token_list(" alice : tok1 , bob : tok2 ");
        assert_eq!(pairs, vec![
            ("alice".to_string(), "tok1".to_string()),
            ("bob".to_string(), "tok2".to_string()),
        ]);
    }
}
