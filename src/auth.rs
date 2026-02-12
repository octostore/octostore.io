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
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct AuthService {
    db: std::sync::Arc<Mutex<Connection>>,
    http_client: Client,
    config: Config,
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

        Ok(Self {
            db: std::sync::Arc::new(Mutex::new(conn)),
            http_client: Client::new(),
            config,
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

        info!("New user created: {}", user.github_username);
        Ok(user)
    }

    pub async fn rotate_token(&self, current_token: &str) -> Result<String> {
        let conn = self.db.lock().unwrap();
        
        let new_token = self.generate_token();
        
        let updated_rows = conn.execute(
            "UPDATE users SET token = ? WHERE token = ?",
            params![new_token, current_token],
        )?;

        if updated_rows == 0 {
            return Err(AppError::Unauthorized);
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

        let conn = self.db.lock().unwrap();
        let user_id: String = conn
            .query_row(
                "SELECT id FROM users WHERE token = ?",
                params![token],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(AppError::Unauthorized)?;

        Ok(Uuid::parse_str(&user_id)?)
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
    
    // Get current token
    let auth_header = headers.get("authorization").unwrap().to_str().unwrap();
    let current_token = auth_header.strip_prefix("Bearer ").unwrap();
    
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