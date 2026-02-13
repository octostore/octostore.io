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
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use std::{collections::HashMap, sync::Mutex};
use dashmap::DashMap;
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct AuthService {
    db: std::sync::Arc<Mutex<Connection>>,
    http_client: Client,
    config: Config,
    /// Cache: token → user_id (avoids SQLite mutex on every request)
    token_cache: std::sync::Arc<DashMap<String, Uuid>>,
}

#[derive(Deserialize)]
pub struct GitHubCallbackQuery {
    code: String,
    state: Option<String>,
}

impl AuthService {
    pub fn new(config: Config) -> Result<Self> {
        let conn = Connection::open(&config.database_url)?;
        
        // Create tables if they don't exist
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

        // Initialize fencing counter if not exists
        conn.execute(
            "INSERT OR IGNORE INTO fencing_counter (id, counter) VALUES (1, 0)",
            [],
        )?;

        info!("Database initialized at: {}", config.database_url);

        // Pre-load existing tokens into cache
        let token_cache = DashMap::new();
        {
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
            db: std::sync::Arc::new(Mutex::new(conn)),
            http_client: Client::new(),
            config,
            token_cache: std::sync::Arc::new(token_cache),
        })
    }

    pub fn github_auth_url(&self) -> String {
        format!(
            "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=user:email",
            self.config.github_client_id,
            urlencoding::encode(&self.config.github_redirect_uri)
        )
    }

    pub async fn handle_github_callback(&self, query: Query<GitHubCallbackQuery>) -> Result<AuthTokenResponse> {
        let code = &query.code;
        
        // Exchange code for access token
        let token_response = self.exchange_code_for_token(code).await?;
        
        // Get GitHub user info
        let github_user = self.get_github_user(&token_response.access_token).await?;
        
        // Create or get user
        let user = self.create_or_get_user(github_user).await?;
        
        Ok(AuthTokenResponse {
            token: user.token,
            user_id: user.id,
            github_username: user.github_username,
        })
    }

    async fn exchange_code_for_token(&self, code: &str) -> Result<GitHubTokenResponse> {
        let mut params = HashMap::new();
        params.insert("client_id", self.config.github_client_id.as_str());
        params.insert("client_secret", self.config.github_client_secret.as_str());
        params.insert("code", code);

        let response = self
            .http_client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AppError::Internal(anyhow::anyhow!("GitHub token exchange failed")));
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
            return Err(AppError::Internal(anyhow::anyhow!("Failed to get GitHub user")));
        }

        let github_user: GitHubUser = response.json().await?;
        Ok(github_user)
    }

    async fn create_or_get_user(&self, github_user: GitHubUser) -> Result<User> {
        let conn = self.db.lock().unwrap();
        
        // Check if user already exists
        let existing_user: Option<User> = conn
            .query_row(
                "SELECT id, github_id, github_username, token, created_at FROM users WHERE github_id = ?",
                params![github_user.id],
                |row| {
                    Ok(User {
                        id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap(),
                        github_id: row.get(1)?,
                        github_username: row.get(2)?,
                        token: row.get(3)?,
                        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(4)?)
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

        // Create new user
        let user_id = Uuid::new_v4();
        let token = self.generate_token();
        let created_at = Utc::now();

        conn.execute(
            "INSERT INTO users (id, github_id, github_username, token, created_at) VALUES (?, ?, ?, ?, ?)",
            params![
                user_id.to_string(),
                github_user.id,
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

        // Add to token cache
        self.token_cache.insert(user.token.clone(), user.id);
        
        info!("New user created: {}", user.github_username);
        Ok(user)
    }

    pub async fn rotate_token(&self, current_token: &str) -> Result<String> {
        // Get user_id before rotating (for cache update)
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

        // Update cache: remove old token, add new one
        self.token_cache.remove(current_token);
        if let Some(uid) = user_id {
            self.token_cache.insert(new_token.clone(), uid);
        }

        Ok(new_token)
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Uuid> {
        let auth_header = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .ok_or(AppError::MissingAuth)?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or(AppError::MissingAuth)?;

        // Check cache first (lock-free, O(1))
        if let Some(user_id) = self.token_cache.get(token) {
            return Ok(*user_id);
        }

        // Cache miss — fall back to SQLite
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
        // Populate cache
        self.token_cache.insert(token.to_string(), uuid);
        Ok(uuid)
    }

    fn generate_token(&self) -> String {
        let mut rng = rand::thread_rng();
        let token_bytes: [u8; 32] = rng.gen();
        base64::engine::general_purpose::STANDARD.encode(token_bytes)
    }

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
        let conn = self.db.lock().unwrap();
        let username: Option<String> = conn.query_row(
            "SELECT github_username FROM users WHERE id = ?",
            params![user_id],
            |row| row.get(0),
        ).optional()?;
        Ok(username)
    }

    pub fn get_all_users(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, github_username, created_at FROM users")?;
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

// Route handlers  
pub async fn github_auth(State(state): State<crate::AppState>) -> Redirect {
    let url = state.auth_service.github_auth_url();
    Redirect::permanent(&url)
}

pub async fn github_callback(
    State(state): State<crate::AppState>,
    query: Query<GitHubCallbackQuery>,
) -> Result<Redirect> {
    let response = state.auth_service.handle_github_callback(query).await?;
    
    // Redirect to the dashboard with token and user info as URL parameters
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
    
    // Get user info for response
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    fn create_test_config() -> Config {
        Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: ":memory:".to_string(), // Use in-memory SQLite for tests
            github_client_id: "test_client_id".to_string(),
            github_client_secret: "test_client_secret".to_string(),
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: Some("test_admin_key".to_string()),
        }
    }

    fn create_test_auth_service() -> AuthService {
        let config = create_test_config();
        AuthService::new(config).unwrap()
    }

    #[test]
    fn test_auth_service_new() {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();
        
        let mut config = create_test_config();
        config.database_url = db_path.to_string();
        
        let auth_service = AuthService::new(config).unwrap();
        
        // Check that tables were created
        let conn = auth_service.db.lock().unwrap();
        
        // Check users table
        let table_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='users'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(table_exists, 1);
        
        // Check fencing_counter table
        let table_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fencing_counter'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(table_exists, 1);
        
        // Check fencing counter was initialized
        let counter: u64 = conn.query_row(
            "SELECT counter FROM fencing_counter WHERE id = 1",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(counter, 0);
    }

    #[test]
    fn test_github_auth_url() {
        let auth_service = create_test_auth_service();
        let url = auth_service.github_auth_url();
        
        assert!(url.contains("https://github.com/login/oauth/authorize"));
        assert!(url.contains("client_id=test_client_id"));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("scope=user%3Aemail"));
    }

    #[test]
    fn test_generate_token() {
        let auth_service = create_test_auth_service();
        
        let token1 = auth_service.generate_token();
        let token2 = auth_service.generate_token();
        
        // Tokens should be different
        assert_ne!(token1, token2);
        
        // Tokens should be base64 encoded (check for typical base64 chars)
        assert!(token1.len() > 0);
        assert!(token1.chars().all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '+' || c == '='));
    }

    #[test]
    fn test_create_or_get_user_new() {
        let auth_service = create_test_auth_service();
        
        let github_user = GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let user = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
        
        assert_eq!(user.github_id, 12345);
        assert_eq!(user.github_username, "testuser");
        assert!(!user.token.is_empty());
        assert_eq!(user.id.to_string().len(), 36); // UUID length
        
        // Check that user was saved to database
        let conn = auth_service.db.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE github_id = ?",
            params![12345],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1);
        
        // Check token cache
        assert!(auth_service.token_cache.contains_key(&user.token));
        assert_eq!(auth_service.token_cache.get(&user.token).unwrap().value(), &user.id);
    }

    #[test]
    fn test_create_or_get_user_existing() {
        let auth_service = create_test_auth_service();
        
        let github_user = GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        
        let runtime = tokio::runtime::Runtime::new().unwrap();
        
        // Create user first time
        let user1 = runtime.block_on(auth_service.create_or_get_user(github_user.clone())).unwrap();
        
        // Create same user again - should return existing
        let user2 = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
        
        assert_eq!(user1.id, user2.id);
        assert_eq!(user1.github_id, user2.github_id);
        assert_eq!(user1.token, user2.token);
        assert_eq!(user1.created_at, user2.created_at);
        
        // Should still be only one user in database
        let conn = auth_service.db.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_authenticate_valid_token() {
        let auth_service = create_test_auth_service();
        
        // Create a user first
        let github_user = GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let user = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
        
        // Test authentication with valid token
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", user.token).parse().unwrap());
        
        let user_id = auth_service.authenticate(&headers).unwrap();
        assert_eq!(user_id, user.id);
    }

    #[test]
    fn test_authenticate_invalid_token() {
        let auth_service = create_test_auth_service();
        
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer invalid_token".parse().unwrap());
        
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::Unauthorized));
    }

    #[test]
    fn test_authenticate_missing_header() {
        let auth_service = create_test_auth_service();
        
        let headers = HeaderMap::new();
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::MissingAuth));
    }

    #[test]
    fn test_authenticate_invalid_format() {
        let auth_service = create_test_auth_service();
        
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "InvalidFormat token".parse().unwrap());
        
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::MissingAuth));
    }

    #[test]
    fn test_authenticate_cache_population() {
        let auth_service = create_test_auth_service();
        
        // Create user but clear cache to simulate cache miss
        let github_user = GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let user = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
        
        // Clear the cache
        auth_service.token_cache.clear();
        assert!(!auth_service.token_cache.contains_key(&user.token));
        
        // Authenticate should populate cache
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", user.token).parse().unwrap());
        
        let user_id = auth_service.authenticate(&headers).unwrap();
        assert_eq!(user_id, user.id);
        
        // Cache should now contain the token
        assert!(auth_service.token_cache.contains_key(&user.token));
        assert_eq!(auth_service.token_cache.get(&user.token).unwrap().value(), &user.id);
    }

    #[test]
    fn test_rotate_token() {
        let auth_service = create_test_auth_service();
        
        // Create a user first
        let github_user = GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let user = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
        let original_token = user.token.clone();
        
        // Rotate token
        let new_token = runtime.block_on(auth_service.rotate_token(&original_token)).unwrap();
        assert_ne!(original_token, new_token);
        
        // Old token should no longer work
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", original_token).parse().unwrap());
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
        
        // New token should work
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", new_token).parse().unwrap());
        let user_id = auth_service.authenticate(&headers).unwrap();
        assert_eq!(user_id, user.id);
        
        // Cache should be updated
        assert!(!auth_service.token_cache.contains_key(&original_token));
        assert!(auth_service.token_cache.contains_key(&new_token));
    }

    #[test]
    fn test_rotate_token_invalid() {
        let auth_service = create_test_auth_service();
        
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(auth_service.rotate_token("invalid_token"));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AppError::Unauthorized));
    }

    #[test]
    fn test_fencing_counter() {
        let auth_service = create_test_auth_service();
        
        // Initial counter should be 0
        let counter = auth_service.load_fencing_counter().unwrap();
        assert_eq!(counter, 0);
        
        // Update counter
        auth_service.save_fencing_counter(42).unwrap();
        let counter = auth_service.load_fencing_counter().unwrap();
        assert_eq!(counter, 42);
        
        // Update again
        auth_service.save_fencing_counter(100).unwrap();
        let counter = auth_service.load_fencing_counter().unwrap();
        assert_eq!(counter, 100);
    }

    #[test]
    fn test_get_user_by_id() {
        let auth_service = create_test_auth_service();
        
        // Create a user
        let github_user = GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let user = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
        
        // Test getting existing user
        let username = auth_service.get_user_by_id(&user.id.to_string()).unwrap();
        assert_eq!(username, Some("testuser".to_string()));
        
        // Test getting non-existent user
        let random_id = Uuid::new_v4();
        let username = auth_service.get_user_by_id(&random_id.to_string()).unwrap();
        assert_eq!(username, None);
    }

    #[test]
    fn test_get_all_users() {
        let auth_service = create_test_auth_service();
        
        // Initially no users
        let users = auth_service.get_all_users().unwrap();
        assert_eq!(users.len(), 0);
        
        // Create some users
        let runtime = tokio::runtime::Runtime::new().unwrap();
        
        let user1 = GitHubUser { id: 1, login: "user1".to_string() };
        let user2 = GitHubUser { id: 2, login: "user2".to_string() };
        
        runtime.block_on(auth_service.create_or_get_user(user1)).unwrap();
        runtime.block_on(auth_service.create_or_get_user(user2)).unwrap();
        
        // Get all users
        let users = auth_service.get_all_users().unwrap();
        assert_eq!(users.len(), 2);
        
        // Check user data
        let usernames: Vec<&str> = users.iter()
            .map(|u| u["github_username"].as_str().unwrap())
            .collect();
        assert!(usernames.contains(&"user1"));
        assert!(usernames.contains(&"user2"));
        
        // Check that each user has required fields
        for user in &users {
            assert!(user["id"].is_string());
            assert!(user["github_username"].is_string());
            assert!(user["created_at"].is_string());
        }
    }

    #[test]
    fn test_token_cache_preload() {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();
        
        let mut config = create_test_config();
        config.database_url = db_path.to_string();
        
        // Create initial auth service and add some users
        {
            let auth_service = AuthService::new(config.clone()).unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            
            let user1 = GitHubUser { id: 1, login: "user1".to_string() };
            let user2 = GitHubUser { id: 2, login: "user2".to_string() };
            
            runtime.block_on(auth_service.create_or_get_user(user1)).unwrap();
            runtime.block_on(auth_service.create_or_get_user(user2)).unwrap();
        }
        
        // Create new auth service - should preload existing tokens
        let auth_service = AuthService::new(config).unwrap();
        assert_eq!(auth_service.token_cache.len(), 2);
    }

    #[test]
    fn test_concurrent_token_operations() {
        use std::sync::Arc;
        use std::thread;
        
        let auth_service = Arc::new(create_test_auth_service());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        
        // Create some users first
        let mut user_tokens = Vec::new();
        for i in 0..5 {
            let github_user = GitHubUser {
                id: i,
                login: format!("user{}", i),
            };
            let user = runtime.block_on(auth_service.create_or_get_user(github_user)).unwrap();
            user_tokens.push(user.token);
        }
        
        // Test concurrent authentication
        let mut handles = vec![];
        for token in user_tokens {
            let auth_service_clone = auth_service.clone();
            let handle = thread::spawn(move || {
                for _ in 0..10 {
                    let mut headers = HeaderMap::new();
                    headers.insert("authorization", format!("Bearer {}", token).parse().unwrap());
                    let result = auth_service_clone.authenticate(&headers);
                    assert!(result.is_ok());
                }
            });
            handles.push(handle);
        }
        
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_database_error_handling() {
        // Create auth service with invalid database path
        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: "/invalid/path/database.db".to_string(),
            github_client_id: "test_client_id".to_string(),
            github_client_secret: "test_client_secret".to_string(),
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: Some("test_admin_key".to_string()),
        };
        
        let result = AuthService::new(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_token_cache_isolation() {
        let auth_service = create_test_auth_service();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        
        // Create users for different scenarios
        let user1 = GitHubUser { id: 1, login: "user1".to_string() };
        let user2 = GitHubUser { id: 2, login: "user2".to_string() };
        
        let user1_obj = runtime.block_on(auth_service.create_or_get_user(user1)).unwrap();
        let user2_obj = runtime.block_on(auth_service.create_or_get_user(user2)).unwrap();
        
        // Verify cache contains both tokens
        assert!(auth_service.token_cache.contains_key(&user1_obj.token));
        assert!(auth_service.token_cache.contains_key(&user2_obj.token));
        
        // Rotate user1's token
        let new_token = runtime.block_on(auth_service.rotate_token(&user1_obj.token)).unwrap();
        
        // Cache should be updated correctly
        assert!(!auth_service.token_cache.contains_key(&user1_obj.token)); // Old token removed
        assert!(auth_service.token_cache.contains_key(&new_token));         // New token added
        assert!(auth_service.token_cache.contains_key(&user2_obj.token));  // Other user unaffected
        
        // User IDs should still map correctly
        assert_eq!(auth_service.token_cache.get(&new_token).unwrap().value(), &user1_obj.id);
        assert_eq!(auth_service.token_cache.get(&user2_obj.token).unwrap().value(), &user2_obj.id);
    }

    #[test]
    fn test_edge_cases() {
        let auth_service = create_test_auth_service();
        
        // Test empty authorization header value
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "".parse().unwrap());
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
        
        // Test malformed bearer token
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer".parse().unwrap()); // No space or token
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
        
        // Test garbage authorization header
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer !!!invalid!!!".parse().unwrap());
        let result = auth_service.authenticate(&headers);
        assert!(result.is_err());
    }

    #[test]
    fn test_github_callback_query_deserialization() {
        // Test with state
        let json = r#"{"code":"auth_code_123","state":"random_state"}"#;
        let query: GitHubCallbackQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.code, "auth_code_123");
        assert_eq!(query.state, Some("random_state".to_string()));
        
        // Test without state
        let json = r#"{"code":"auth_code_456"}"#;
        let query: GitHubCallbackQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.code, "auth_code_456");
        assert_eq!(query.state, None);
    }

    #[test]
    fn test_auth_service_config_validation() {
        // Test that config values are properly stored
        let config = Config {
            bind_addr: "127.0.0.1:8080".to_string(),
            database_url: ":memory:".to_string(),
            github_client_id: "custom_client_id".to_string(),
            github_client_secret: "custom_secret".to_string(),
            github_redirect_uri: "https://custom.example.com/callback".to_string(),
            admin_key: Some("custom_admin_key".to_string()),
        };
        
        let auth_service = AuthService::new(config.clone()).unwrap();
        
        // Check that config is stored correctly
        assert_eq!(auth_service.config.github_client_id, "custom_client_id");
        assert_eq!(auth_service.config.github_client_secret, "custom_secret");
        assert_eq!(auth_service.config.github_redirect_uri, "https://custom.example.com/callback");
        assert_eq!(auth_service.config.admin_key, Some("custom_admin_key".to_string()));
        
        // Check that GitHub URL uses the correct config
        let url = auth_service.github_auth_url();
        assert!(url.contains("client_id=custom_client_id"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fcustom.example.com%2Fcallback"));
    }
}