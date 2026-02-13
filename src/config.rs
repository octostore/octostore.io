use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub github_client_id: String,
    pub github_client_secret: String,
    pub github_redirect_uri: String,
    pub admin_key: Option<String>,
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
            admin_key: env::var("ADMIN_KEY").ok(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    // Helper to backup and restore environment variables
    fn with_env_vars<F>(vars: Vec<(&str, Option<&str>)>, test_fn: F) 
    where F: FnOnce() {
        let mut backup = Vec::new();
        
        // Backup current values and set test values
        for (key, value) in &vars {
            backup.push((key.to_string(), env::var(key).ok()));
            match value {
                Some(val) => env::set_var(key, val),
                None => env::remove_var(key),
            }
        }
        
        // Run the test
        test_fn();
        
        // Restore original values
        for (key, original_value) in backup {
            match original_value {
                Some(val) => env::set_var(key, val),
                None => env::remove_var(key),
            }
        }
    }

    #[test]
    fn test_config_from_env_with_all_vars_set() {
        with_env_vars(vec![
            ("BIND_ADDR", Some("127.0.0.1:8080")),
            ("DATABASE_URL", Some("/tmp/test.db")),
            ("GITHUB_CLIENT_ID", Some("test_client_id")),
            ("GITHUB_CLIENT_SECRET", Some("test_client_secret")),
            ("GITHUB_REDIRECT_URI", Some("https://example.com/callback")),
            ("ADMIN_KEY", Some("test_admin_key")),
        ], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.bind_addr, "127.0.0.1:8080");
            assert_eq!(config.database_url, "/tmp/test.db");
            assert_eq!(config.github_client_id, "test_client_id");
            assert_eq!(config.github_client_secret, "test_client_secret");
            assert_eq!(config.github_redirect_uri, "https://example.com/callback");
            assert_eq!(config.admin_key, Some("test_admin_key".to_string()));
        });
    }

    #[test]
    fn test_config_from_env_with_defaults() {
        with_env_vars(vec![
            ("BIND_ADDR", None),
            ("DATABASE_URL", None),
            ("GITHUB_CLIENT_ID", Some("test_client_id")),
            ("GITHUB_CLIENT_SECRET", Some("test_client_secret")),
            ("GITHUB_REDIRECT_URI", None),
            ("ADMIN_KEY", None),
        ], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.bind_addr, "0.0.0.0:3000");
            assert_eq!(config.database_url, "octostore.db");
            assert_eq!(config.github_client_id, "test_client_id");
            assert_eq!(config.github_client_secret, "test_client_secret");
            assert_eq!(config.github_redirect_uri, "http://localhost:3000/auth/github/callback");
            assert_eq!(config.admin_key, None);
        });
    }

    #[test]
    fn test_config_missing_github_client_id() {
        with_env_vars(vec![
            ("GITHUB_CLIENT_ID", None),
            ("GITHUB_CLIENT_SECRET", Some("test_client_secret")),
        ], || {
            let result = Config::from_env();
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("GITHUB_CLIENT_ID must be set"));
        });
    }

    #[test]
    fn test_config_missing_github_client_secret() {
        with_env_vars(vec![
            ("GITHUB_CLIENT_ID", Some("test_client_id")),
            ("GITHUB_CLIENT_SECRET", None),
        ], || {
            let result = Config::from_env();
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("GITHUB_CLIENT_SECRET must be set"));
        });
    }

    #[test]
    fn test_config_missing_both_required_vars() {
        with_env_vars(vec![
            ("GITHUB_CLIENT_ID", None),
            ("GITHUB_CLIENT_SECRET", None),
        ], || {
            let result = Config::from_env();
            assert!(result.is_err());
            // Should fail on the first missing variable
            assert!(result.unwrap_err().to_string().contains("GITHUB_CLIENT_ID must be set"));
        });
    }

}