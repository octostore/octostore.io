use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    /// GitHub OAuth credentials — both must be set to enable GitHub auth.
    /// When absent the server falls back to local-auth mode.
    pub github_client_id: Option<String>,
    pub github_client_secret: Option<String>,
    pub github_redirect_uri: String,
    pub admin_key: Option<String>,
    /// Comma-separated static tokens for local-auth mode.
    /// Format: `user1:token1,user2:token2`  or bare `token` (username = token value).
    /// Tokens are seeded into the DB on startup so the normal Bearer-token path
    /// works unchanged.
    pub static_tokens: Option<String>,
    /// Path to a newline-delimited file of `user:token` pairs (# = comment).
    /// Loaded in addition to STATIC_TOKENS if both are set.
    pub static_tokens_file: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Config {
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string()),
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "octostore.db".to_string()),
            github_client_id: env::var("GITHUB_CLIENT_ID").ok(),
            github_client_secret: env::var("GITHUB_CLIENT_SECRET").ok(),
            github_redirect_uri: env::var("GITHUB_REDIRECT_URI")
                .unwrap_or_else(|_| "http://localhost:3000/auth/github/callback".to_string()),
            admin_key: env::var("ADMIN_KEY").ok(),
            static_tokens: env::var("STATIC_TOKENS").ok(),
            static_tokens_file: env::var("STATIC_TOKENS_FILE").ok(),
        })
    }

    /// Returns true when GitHub OAuth is fully configured.
    pub fn is_github_enabled(&self) -> bool {
        self.github_client_id.is_some() && self.github_client_secret.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn with_env_vars<F>(vars: Vec<(&str, Option<&str>)>, test_fn: F)
    where
        F: FnOnce(),
    {
        let mut backup = Vec::new();
        for (key, value) in &vars {
            backup.push((key.to_string(), env::var(key).ok()));
            match value {
                Some(val) => env::set_var(key, val),
                None => env::remove_var(key),
            }
        }
        test_fn();
        for (key, original_value) in backup {
            match original_value {
                Some(val) => env::set_var(key, val),
                None => env::remove_var(key),
            }
        }
    }

    #[test]
    fn test_config_github_enabled() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", Some("cid")),
                ("GITHUB_CLIENT_SECRET", Some("csec")),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert!(c.is_github_enabled());
            },
        );
    }

    #[test]
    fn test_config_github_disabled_when_missing() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", None),
                ("GITHUB_CLIENT_SECRET", None),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert!(!c.is_github_enabled());
            },
        );
    }

    #[test]
    fn test_config_github_disabled_when_partial() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", Some("cid")),
                ("GITHUB_CLIENT_SECRET", None),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert!(!c.is_github_enabled());
            },
        );
    }

    #[test]
    fn test_config_static_tokens() {
        with_env_vars(
            vec![("STATIC_TOKENS", Some("alice:tok1,bob:tok2"))],
            || {
                let c = Config::from_env().unwrap();
                assert_eq!(c.static_tokens.as_deref(), Some("alice:tok1,bob:tok2"));
            },
        );
    }

    #[test]
    fn test_config_defaults() {
        with_env_vars(
            vec![
                ("BIND_ADDR", None),
                ("DATABASE_URL", None),
                ("GITHUB_CLIENT_ID", None),
                ("GITHUB_CLIENT_SECRET", None),
                ("GITHUB_REDIRECT_URI", None),
                ("ADMIN_KEY", None),
                ("STATIC_TOKENS", None),
                ("STATIC_TOKENS_FILE", None),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert_eq!(c.bind_addr, "0.0.0.0:3000");
                assert_eq!(c.database_url, "octostore.db");
                assert!(c.github_client_id.is_none());
                assert!(c.github_client_secret.is_none());
                assert!(c.static_tokens.is_none());
                assert!(c.static_tokens_file.is_none());
                assert!(!c.is_github_enabled());
            },
        );
    }
}
