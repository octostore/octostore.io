use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub github_client_id: String,
    pub github_client_secret: String,
    pub github_redirect_uri: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Config {
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string()),
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "octostore.db".to_string()),
            github_client_id: env::var("GITHUB_CLIENT_ID")
                .map_err(|_| anyhow::anyhow!("GITHUB_CLIENT_ID must be set"))?,
            github_client_secret: env::var("GITHUB_CLIENT_SECRET")
                .map_err(|_| anyhow::anyhow!("GITHUB_CLIENT_SECRET must be set"))?,
            github_redirect_uri: env::var("GITHUB_REDIRECT_URI")
                .unwrap_or_else(|_| "http://localhost:3000/auth/github/callback".to_string()),
        })
    }
}