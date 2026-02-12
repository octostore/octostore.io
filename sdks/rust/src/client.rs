//! OctoStore client implementation.

use std::time::Duration;

use regex::Regex;
use reqwest::{header::HeaderValue, StatusCode};
use serde_json::Value;
use url::Url;

use crate::{
    error::{Error, LockHeldError, Result},
    types::*,
};

/// OctoStore distributed lock service client.
#[derive(Debug, Clone)]
pub struct Client {
    base_url: Url,
    token: Option<String>,
    http_client: reqwest::Client,
}

impl Client {
    /// Create a new OctoStore client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The base URL of the OctoStore API
    /// * `token` - Bearer token for authentication
    ///
    /// # Errors
    ///
    /// Returns an error if the base URL is invalid.
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let base_url = Url::parse(base_url)?;
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            base_url,
            token: Some(token.to_string()),
            http_client,
        })
    }

    /// Create a new OctoStore client without authentication.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The base URL of the OctoStore API
    ///
    /// # Errors
    ///
    /// Returns an error if the base URL is invalid.
    pub fn new_without_auth(base_url: &str) -> Result<Self> {
        let base_url = Url::parse(base_url)?;
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            base_url,
            token: None,
            http_client,
        })
    }

    /// Set the timeout for HTTP requests.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.http_client = reqwest::Client::builder()
            .timeout(timeout)
            .build()?;
        Ok(self)
    }

    /// Set the authentication token.
    pub fn set_token(&mut self, token: String) {
        self.token = Some(token);
    }

    /// Get the current authentication token.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Validate lock name format.
    fn validate_lock_name(name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 128 {
            return Err(Error::validation("Lock name must be 1-128 characters"));
        }

        let re = Regex::new(r"^[a-zA-Z0-9.-]+$").unwrap();
        if !re.is_match(name) {
            return Err(Error::validation(
                "Lock name can only contain alphanumeric characters, hyphens, and dots",
            ));
        }

        Ok(())
    }

    /// Validate TTL value.
    fn validate_ttl(ttl: u32) -> Result<()> {
        if !(1..=3600).contains(&ttl) {
            return Err(Error::validation("TTL must be between 1 and 3600 seconds"));
        }
        Ok(())
    }

    /// Make an HTTP request and handle common errors.
    async fn request<T>(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let url = self.base_url.join(path)?;
        let mut request = self.http_client.request(method, url);

        if let Some(token) = &self.token {
            let auth_header = HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| Error::other(format!("Invalid token format: {}", e)))?;
            request = request.header(reqwest::header::AUTHORIZATION, auth_header);
        }

        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request.send().await?;
        self.handle_response(response).await
    }

    /// Handle HTTP response and parse JSON or handle errors.
    async fn handle_response<T>(&self, response: reqwest::Response) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let status = response.status();
        
        match status {
            StatusCode::UNAUTHORIZED => {
                return Err(Error::authentication("Invalid or missing authentication token"));
            }
            StatusCode::CONFLICT => {
                // This might be a lock acquisition conflict
                let error_data: Value = response.json().await.unwrap_or_default();
                if let (Some(holder_id), Some(expires_at)) = (
                    error_data.get("holder_id").and_then(|v| v.as_str()),
                    error_data.get("expires_at").and_then(|v| v.as_str()),
                ) {
                    return Err(Error::LockHeld(LockHeldError::new(
                        "lock".to_string(),
                        Some(holder_id.to_string()),
                        Some(expires_at.to_string()),
                    )));
                }
            }
            _ if status.is_client_error() || status.is_server_error() => {
                let error_data: Value = response.json().await.unwrap_or_default();
                let message = error_data
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&format!("HTTP {}", status.as_u16()));
                return Err(Error::lock(message, status.as_u16()));
            }
            _ => {}
        }

        // Handle successful responses
        let text = response.text().await?;
        
        if text.is_empty() {
            // For responses with no body, try to deserialize an empty object
            return serde_json::from_str("{}").map_err(Error::from);
        }

        // Check if it's JSON or plain text
        if text.starts_with('{') || text.starts_with('[') {
            serde_json::from_str(&text).map_err(Error::from)
        } else {
            // For plain text responses (like health check), try to parse as string
            serde_json::from_value(Value::String(text.trim_matches('"').to_string()))
                .map_err(Error::from)
        }
    }

    /// Check API health status.
    pub async fn health(&self) -> Result<String> {
        self.request(reqwest::Method::GET, "/health", None).await
    }

    /// Acquire a distributed lock.
    ///
    /// # Arguments
    ///
    /// * `name` - Lock name (alphanumeric, hyphens, dots only, max 128 chars)
    /// * `ttl` - Time-to-live in seconds (1-3600)
    ///
    /// # Errors
    ///
    /// Returns `Error::LockHeld` if the lock is already held by another process.
    pub async fn acquire_lock(&self, name: &str, ttl: u32) -> Result<AcquireResult> {
        Self::validate_lock_name(name)?;
        Self::validate_ttl(ttl)?;

        let body = serde_json::to_value(AcquireRequest { ttl_seconds: ttl })?;
        let path = format!("/locks/{}/acquire", urlencoding::encode(name));
        
        self.request(reqwest::Method::POST, &path, Some(body)).await
    }

    /// Release a distributed lock.
    ///
    /// # Arguments
    ///
    /// * `name` - Lock name
    /// * `lease_id` - Lease ID from the acquire operation
    pub async fn release_lock(&self, name: &str, lease_id: &str) -> Result<()> {
        Self::validate_lock_name(name)?;

        let body = serde_json::to_value(ReleaseRequest {
            lease_id: lease_id.to_string(),
        })?;
        let path = format!("/locks/{}/release", urlencoding::encode(name));
        
        self.request::<Value>(reqwest::Method::POST, &path, Some(body)).await?;
        Ok(())
    }

    /// Renew a distributed lock.
    ///
    /// # Arguments
    ///
    /// * `name` - Lock name
    /// * `lease_id` - Lease ID from the acquire operation
    /// * `ttl` - New time-to-live in seconds (1-3600)
    pub async fn renew_lock(&self, name: &str, lease_id: &str, ttl: u32) -> Result<RenewResult> {
        Self::validate_lock_name(name)?;
        Self::validate_ttl(ttl)?;

        let body = serde_json::to_value(RenewRequest {
            lease_id: lease_id.to_string(),
            ttl_seconds: ttl,
        })?;
        let path = format!("/locks/{}/renew", urlencoding::encode(name));
        
        self.request(reqwest::Method::POST, &path, Some(body)).await
    }

    /// Get the current status of a lock.
    ///
    /// # Arguments
    ///
    /// * `name` - Lock name
    pub async fn get_lock_status(&self, name: &str) -> Result<LockInfo> {
        Self::validate_lock_name(name)?;

        let path = format!("/locks/{}", urlencoding::encode(name));
        self.request(reqwest::Method::GET, &path, None).await
    }

    /// List all locks owned by the current user.
    pub async fn list_locks(&self) -> Result<Vec<UserLock>> {
        let response: ListLocksResponse = self.request(reqwest::Method::GET, "/locks", None).await?;
        Ok(response.locks)
    }

    /// Rotate the authentication token.
    ///
    /// This operation updates the client's token automatically.
    pub async fn rotate_token(&mut self) -> Result<TokenInfo> {
        let token_info: TokenInfo = self
            .request(reqwest::Method::POST, "/auth/token/rotate", None)
            .await?;

        // Update the client's token
        self.token = Some(token_info.token.clone());
        
        Ok(token_info)
    }
}

// Add urlencoding functionality since it's not in std
mod urlencoding {
    pub fn encode(input: &str) -> String {
        input
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u8)
                }
            })
            .collect()
    }
}