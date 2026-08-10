use anyhow::{bail, Context};
use reqwest::Url;
use std::{env, fs};

const HOSTED_API_BASE_URL: &str = "https://api.octostore.io";
const HOSTED_DASHBOARD_URL: &str = "https://octostore.io/dashboard.html";

fn optional_env(name: &str) -> anyhow::Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow::anyhow!("{name} is not valid Unicode: {error}")),
    }
}

fn optional_nonempty_env(name: &str) -> anyhow::Result<Option<String>> {
    match optional_env(name)? {
        Some(value) if value.trim().is_empty() => bail!("{name} must not be empty when set"),
        value => Ok(value),
    }
}

fn boolean_env(name: &str, default: bool) -> anyhow::Result<bool> {
    let Some(value) = optional_env(name)? else {
        return Ok(default);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => bail!("{name} must be one of true, false, 1, 0, on, off, yes, or no"),
    }
}

fn positive_usize_env(name: &str, default: usize) -> anyhow::Result<usize> {
    let Some(value) = optional_env(name)? else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<usize>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn positive_u32_env(name: &str, default: u32) -> anyhow::Result<u32> {
    let Some(value) = optional_env(name)? else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<u32>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn positive_u64_env(name: &str, default: u64) -> anyhow::Result<u64> {
    let Some(value) = optional_env(name)? else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn is_loopback_host(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn validate_browser_url(name: &str, raw: &str, expected_path: Option<&str>) -> anyhow::Result<Url> {
    let url = Url::parse(raw).with_context(|| format!("{name} must be an absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{name} must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("{name} must not contain a query or fragment");
    }
    if let Some(path) = expected_path {
        if url.path() != path {
            bail!("{name} path must be {path}");
        }
    }
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&url) => {}
        _ => bail!("{name} must use HTTPS, except for an explicit loopback host"),
    }
    if url.host_str().is_none() {
        bail!("{name} must include a host");
    }
    Ok(url)
}

pub(crate) fn parse_static_token_list(
    raw: &str,
    source: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();

    for (index, entry) in raw
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && !entry.starts_with('#'))
        .enumerate()
    {
        let (username, token) = match entry.split_once(':') {
            Some((username, token)) => (username.trim(), token.trim()),
            None => (entry, entry),
        };

        if username.is_empty() {
            bail!("{source} contains an empty username at entry {}", index + 1);
        }
        if token.is_empty() {
            bail!("{source} contains an empty token at entry {}", index + 1);
        }
        if token.chars().any(char::is_whitespace) {
            bail!(
                "{source} contains whitespace in a token at entry {}",
                index + 1
            );
        }

        pairs.push((username.to_string(), token.to_string()));
    }

    if pairs.is_empty() {
        bail!("{source} must contain at least one token");
    }

    Ok(pairs)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    /// GitHub OAuth credentials — both must be set to enable GitHub auth.
    /// When absent the server falls back to local-auth mode.
    pub github_client_id: Option<String>,
    pub github_client_secret: Option<String>,
    pub github_redirect_uri: String,
    /// Public API origin that issued a browser OAuth handoff. Derived from the
    /// validated GitHub callback URI and carried to the configured dashboard.
    pub oauth_api_base_url: Option<String>,
    /// Dashboard URL that receives the one-time handoff code and issuing API
    /// origin. Required for non-hosted GitHub OAuth deployments.
    pub oauth_dashboard_url: Option<String>,
    pub admin_key: Option<String>,
    /// Username that is granted admin access via OAuth bearer token.
    /// Read from `ADMIN_USERNAME` env var. Falls back to no OAuth-based admin
    /// if not set.
    pub admin_username: Option<String>,
    /// Comma-separated static tokens for local-auth mode.
    /// Format: `user1:token1,user2:token2`  or bare `token` (username = token value).
    /// Tokens are seeded into the DB on startup so the normal Bearer-token path
    /// works unchanged.
    pub static_tokens: Option<String>,
    /// Path to a newline-delimited file of `user:token` pairs (# = comment).
    /// Loaded in addition to STATIC_TOKENS if both are set.
    pub static_tokens_file: Option<String>,
    /// Enables one-time local user enrollment. This is disabled by default and
    /// may only be exposed on an explicit numeric loopback bind address.
    pub local_registration_enabled: bool,
    /// Enables account-free, capability-based leader elections.
    pub public_elections_enabled: bool,
    /// Maximum number of simultaneously active public election rooms.
    pub max_public_elections: usize,
    /// Maximum room-creation and campaign requests accepted per client per minute.
    pub public_election_requests_per_minute: u32,
    /// Maximum simultaneous public election watch streams across the server.
    pub public_election_watch_streams_global: usize,
    /// Maximum simultaneous public election watch streams per admission client.
    pub public_election_watch_streams_per_client: usize,
    /// Maximum lifetime of one public election watch connection.
    pub public_election_watch_max_seconds: u64,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr = optional_env("BIND_ADDR")?.unwrap_or_else(|| "0.0.0.0:3000".to_string());
        let github_client_id = optional_nonempty_env("GITHUB_CLIENT_ID")?;
        let github_client_secret = optional_nonempty_env("GITHUB_CLIENT_SECRET")?;
        if github_client_id.is_some() != github_client_secret.is_some() {
            bail!("GITHUB_CLIENT_ID and GITHUB_CLIENT_SECRET must be set together");
        }
        let github_redirect_uri = optional_env("GITHUB_REDIRECT_URI")?
            .unwrap_or_else(|| "http://localhost:3000/auth/github/callback".to_string());
        let configured_dashboard_url = optional_nonempty_env("OAUTH_DASHBOARD_URL")?;
        let (oauth_api_base_url, oauth_dashboard_url) = if github_client_id.is_some() {
            let callback = validate_browser_url(
                "GITHUB_REDIRECT_URI",
                &github_redirect_uri,
                Some("/auth/github/callback"),
            )?;
            let api_base = callback.origin().ascii_serialization();
            let dashboard = match configured_dashboard_url {
                Some(value) => validate_browser_url("OAUTH_DASHBOARD_URL", &value, None)?,
                None if api_base == HOSTED_API_BASE_URL => Url::parse(HOSTED_DASHBOARD_URL)?,
                None => {
                    bail!(
                        "OAUTH_DASHBOARD_URL is required when GitHub OAuth uses a self-hosted callback"
                    )
                }
            };
            (
                Some(api_base),
                Some(dashboard.as_str().trim_end_matches('/').to_string()),
            )
        } else {
            if configured_dashboard_url.is_some() {
                bail!("OAUTH_DASHBOARD_URL requires GitHub OAuth credentials");
            }
            (None, None)
        };

        let admin_key = optional_nonempty_env("ADMIN_KEY")?;
        let static_tokens = optional_nonempty_env("STATIC_TOKENS")?;
        let mut configured_static_tokens = Vec::new();
        if let Some(raw) = &static_tokens {
            configured_static_tokens.extend(parse_static_token_list(raw, "STATIC_TOKENS")?);
        }

        let static_tokens_file = optional_nonempty_env("STATIC_TOKENS_FILE")?;
        if let Some(path) = &static_tokens_file {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("failed to read configured STATIC_TOKENS_FILE {path}"))?;
            configured_static_tokens
                .extend(parse_static_token_list(&contents, "STATIC_TOKENS_FILE")?);
        }

        if admin_key.as_ref().is_some_and(|admin_key| {
            configured_static_tokens
                .iter()
                .any(|(_, token)| token == admin_key)
        }) {
            bail!("ADMIN_KEY must not match any configured static bearer token");
        }

        let local_registration_enabled = boolean_env("LOCAL_REGISTRATION", false)?;
        if local_registration_enabled {
            if github_client_id.is_some() {
                bail!("LOCAL_REGISTRATION cannot be combined with GitHub OAuth");
            }
            if static_tokens.is_some() || static_tokens_file.is_some() {
                bail!(
                    "LOCAL_REGISTRATION cannot be combined with STATIC_TOKENS or STATIC_TOKENS_FILE"
                );
            }
            let socket = bind_addr.parse::<std::net::SocketAddr>().with_context(|| {
                "LOCAL_REGISTRATION requires BIND_ADDR to use an explicit numeric loopback address"
            })?;
            if !socket.ip().is_loopback() {
                bail!("LOCAL_REGISTRATION requires BIND_ADDR to use a loopback address");
            }
        }

        let admin_username = optional_nonempty_env("ADMIN_USERNAME")?;
        if admin_username.is_some() && github_client_id.is_none() {
            bail!("ADMIN_USERNAME requires GitHub OAuth credentials");
        }

        Ok(Config {
            bind_addr,
            database_url: optional_env("DATABASE_URL")?
                .unwrap_or_else(|| "octostore.db".to_string()),
            github_client_id,
            github_client_secret,
            github_redirect_uri,
            oauth_api_base_url,
            oauth_dashboard_url,
            admin_key,
            admin_username,
            static_tokens,
            static_tokens_file,
            local_registration_enabled,
            public_elections_enabled: boolean_env("PUBLIC_ELECTIONS", true)?,
            max_public_elections: positive_usize_env("MAX_PUBLIC_ELECTIONS", 10_000)?,
            public_election_requests_per_minute: positive_u32_env(
                "PUBLIC_ELECTION_REQUESTS_PER_MINUTE",
                600,
            )?,
            public_election_watch_streams_global: positive_usize_env(
                "PUBLIC_ELECTION_WATCH_STREAMS_GLOBAL",
                1_024,
            )?,
            public_election_watch_streams_per_client: positive_usize_env(
                "PUBLIC_ELECTION_WATCH_STREAMS_PER_CLIENT",
                8,
            )?,
            public_election_watch_max_seconds: positive_u64_env(
                "PUBLIC_ELECTION_WATCH_MAX_SECONDS",
                900,
            )?,
        })
    }

    /// Returns true when GitHub OAuth is fully configured.
    pub fn is_github_enabled(&self) -> bool {
        self.github_client_id.is_some() && self.github_client_secret.is_some()
    }

    pub fn oauth_dashboard_origin(&self) -> Option<String> {
        self.oauth_dashboard_url.as_deref().map(|dashboard| {
            Url::parse(dashboard)
                .expect("OAuth dashboard URL was validated at startup")
                .origin()
                .ascii_serialization()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, io::Write, sync::Mutex};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_vars<F>(mut vars: Vec<(&str, Option<&str>)>, test_fn: F)
    where
        F: FnOnce(),
    {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        for name in [
            "BIND_ADDR",
            "DATABASE_URL",
            "GITHUB_CLIENT_ID",
            "GITHUB_CLIENT_SECRET",
            "GITHUB_REDIRECT_URI",
            "OAUTH_DASHBOARD_URL",
            "ADMIN_KEY",
            "ADMIN_USERNAME",
            "STATIC_TOKENS",
            "STATIC_TOKENS_FILE",
            "LOCAL_REGISTRATION",
            "PUBLIC_ELECTIONS",
            "MAX_PUBLIC_ELECTIONS",
            "PUBLIC_ELECTION_REQUESTS_PER_MINUTE",
            "PUBLIC_ELECTION_WATCH_STREAMS_GLOBAL",
            "PUBLIC_ELECTION_WATCH_STREAMS_PER_CLIENT",
            "PUBLIC_ELECTION_WATCH_MAX_SECONDS",
        ] {
            if !vars.iter().any(|(configured, _)| *configured == name) {
                vars.push((name, None));
            }
        }
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
                (
                    "GITHUB_REDIRECT_URI",
                    Some("https://api.octostore.io/auth/github/callback"),
                ),
                ("OAUTH_DASHBOARD_URL", None),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert!(c.is_github_enabled());
                assert_eq!(
                    c.oauth_api_base_url.as_deref(),
                    Some("https://api.octostore.io")
                );
                assert_eq!(
                    c.oauth_dashboard_url.as_deref(),
                    Some("https://octostore.io/dashboard.html")
                );
            },
        );
    }

    #[test]
    fn test_config_github_disabled_when_missing() {
        with_env_vars(
            vec![("GITHUB_CLIENT_ID", None), ("GITHUB_CLIENT_SECRET", None)],
            || {
                let c = Config::from_env().unwrap();
                assert!(!c.is_github_enabled());
            },
        );
    }

    #[test]
    fn test_config_rejects_partial_github_credentials() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", Some("cid")),
                ("GITHUB_CLIENT_SECRET", None),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("must be set together"));
            },
        );
    }

    #[test]
    fn test_config_requires_github_oauth_for_admin_username() {
        with_env_vars(
            vec![
                ("ADMIN_USERNAME", Some("octoadmin")),
                ("GITHUB_CLIENT_ID", None),
                ("GITHUB_CLIENT_SECRET", None),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("ADMIN_USERNAME requires GitHub OAuth credentials"));
            },
        );
    }

    #[test]
    fn test_config_static_tokens() {
        with_env_vars(vec![("STATIC_TOKENS", Some("alice:tok1,bob:tok2"))], || {
            let c = Config::from_env().unwrap();
            assert_eq!(c.static_tokens.as_deref(), Some("alice:tok1,bob:tok2"));
        });
    }

    #[test]
    fn test_config_rejects_admin_key_collision_with_static_bearer() {
        with_env_vars(
            vec![
                ("ADMIN_KEY", Some("same-secret")),
                ("STATIC_TOKENS", Some("alice:same-secret")),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("ADMIN_KEY must not match"));
                assert!(!error.contains("same-secret"));
            },
        );
    }

    #[test]
    fn test_config_rejects_admin_key_collision_from_static_token_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "alice:file-secret").unwrap();
        let path = file.path().to_str().unwrap();
        with_env_vars(
            vec![
                ("ADMIN_KEY", Some("file-secret")),
                ("STATIC_TOKENS_FILE", Some(path)),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("ADMIN_KEY must not match"));
                assert!(!error.contains("file-secret"));
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
                ("OAUTH_DASHBOARD_URL", None),
                ("ADMIN_KEY", None),
                ("STATIC_TOKENS", None),
                ("STATIC_TOKENS_FILE", None),
                ("LOCAL_REGISTRATION", None),
                ("PUBLIC_ELECTIONS", None),
                ("MAX_PUBLIC_ELECTIONS", None),
                ("PUBLIC_ELECTION_REQUESTS_PER_MINUTE", None),
                ("PUBLIC_ELECTION_WATCH_STREAMS_GLOBAL", None),
                ("PUBLIC_ELECTION_WATCH_STREAMS_PER_CLIENT", None),
                ("PUBLIC_ELECTION_WATCH_MAX_SECONDS", None),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert_eq!(c.bind_addr, "0.0.0.0:3000");
                assert_eq!(c.database_url, "octostore.db");
                assert!(c.github_client_id.is_none());
                assert!(c.github_client_secret.is_none());
                assert!(c.oauth_api_base_url.is_none());
                assert!(c.oauth_dashboard_url.is_none());
                assert!(c.static_tokens.is_none());
                assert!(c.static_tokens_file.is_none());
                assert!(!c.local_registration_enabled);
                assert!(c.public_elections_enabled);
                assert_eq!(c.max_public_elections, 10_000);
                assert_eq!(c.public_election_requests_per_minute, 600);
                assert_eq!(c.public_election_watch_streams_global, 1_024);
                assert_eq!(c.public_election_watch_streams_per_client, 8);
                assert_eq!(c.public_election_watch_max_seconds, 900);
                assert!(!c.is_github_enabled());
            },
        );
    }

    #[test]
    fn test_public_election_config() {
        with_env_vars(
            vec![
                ("PUBLIC_ELECTIONS", Some("false")),
                ("MAX_PUBLIC_ELECTIONS", Some("250")),
                ("PUBLIC_ELECTION_REQUESTS_PER_MINUTE", Some("42")),
                ("PUBLIC_ELECTION_WATCH_STREAMS_GLOBAL", Some("250")),
                ("PUBLIC_ELECTION_WATCH_STREAMS_PER_CLIENT", Some("3")),
                ("PUBLIC_ELECTION_WATCH_MAX_SECONDS", Some("45")),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert!(!c.public_elections_enabled);
                assert_eq!(c.max_public_elections, 250);
                assert_eq!(c.public_election_requests_per_minute, 42);
                assert_eq!(c.public_election_watch_streams_global, 250);
                assert_eq!(c.public_election_watch_streams_per_client, 3);
                assert_eq!(c.public_election_watch_max_seconds, 45);
            },
        );
    }

    #[test]
    fn test_local_registration_requires_an_explicit_loopback_only_configuration() {
        with_env_vars(
            vec![
                ("BIND_ADDR", Some("127.0.0.1:3000")),
                ("GITHUB_CLIENT_ID", None),
                ("GITHUB_CLIENT_SECRET", None),
                ("STATIC_TOKENS", None),
                ("STATIC_TOKENS_FILE", None),
                ("LOCAL_REGISTRATION", Some("true")),
            ],
            || assert!(Config::from_env().unwrap().local_registration_enabled),
        );

        for bind_addr in ["0.0.0.0:3000", "localhost:3000"] {
            with_env_vars(
                vec![
                    ("BIND_ADDR", Some(bind_addr)),
                    ("GITHUB_CLIENT_ID", None),
                    ("GITHUB_CLIENT_SECRET", None),
                    ("STATIC_TOKENS", None),
                    ("STATIC_TOKENS_FILE", None),
                    ("LOCAL_REGISTRATION", Some("true")),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(error.contains("LOCAL_REGISTRATION"));
                    assert!(error.contains("loopback"));
                },
            );
        }
    }

    #[test]
    fn test_self_hosted_oauth_binds_callback_dashboard_and_cors_origin() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", Some("cid")),
                ("GITHUB_CLIENT_SECRET", Some("csec")),
                (
                    "GITHUB_REDIRECT_URI",
                    Some("http://127.0.0.1:4100/auth/github/callback"),
                ),
                (
                    "OAUTH_DASHBOARD_URL",
                    Some("http://localhost:4173/dashboard.html"),
                ),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(
                    config.oauth_api_base_url.as_deref(),
                    Some("http://127.0.0.1:4100")
                );
                assert_eq!(
                    config.oauth_dashboard_url.as_deref(),
                    Some("http://localhost:4173/dashboard.html")
                );
                assert_eq!(
                    config.oauth_dashboard_origin().as_deref(),
                    Some("http://localhost:4173")
                );
            },
        );
    }

    #[test]
    fn test_hosted_oauth_binds_authenticated_locks_to_the_hosted_dashboard() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", Some("cid")),
                ("GITHUB_CLIENT_SECRET", Some("csec")),
                (
                    "GITHUB_REDIRECT_URI",
                    Some("https://api.octostore.io/auth/github/callback"),
                ),
                ("OAUTH_DASHBOARD_URL", None),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(
                    config.oauth_api_base_url.as_deref(),
                    Some("https://api.octostore.io")
                );
                assert_eq!(
                    config.oauth_dashboard_url.as_deref(),
                    Some("https://octostore.io/dashboard.html")
                );
                assert_eq!(
                    config.oauth_dashboard_origin().as_deref(),
                    Some("https://octostore.io")
                );
            },
        );
    }

    #[test]
    fn test_self_hosted_oauth_requires_an_explicit_dashboard() {
        with_env_vars(
            vec![
                ("GITHUB_CLIENT_ID", Some("cid")),
                ("GITHUB_CLIENT_SECRET", Some("csec")),
                (
                    "GITHUB_REDIRECT_URI",
                    Some("https://api.example.test/auth/github/callback"),
                ),
                ("OAUTH_DASHBOARD_URL", None),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("OAUTH_DASHBOARD_URL"));
                assert!(error.contains("self-hosted"));
            },
        );
    }

    #[test]
    fn test_oauth_urls_fail_closed_on_unsafe_or_ambiguous_authorities() {
        for (name, value, expected) in [
            (
                "GITHUB_REDIRECT_URI",
                "http://api.example.test/auth/github/callback",
                "HTTPS",
            ),
            (
                "GITHUB_REDIRECT_URI",
                "https://api.example.test/not-the-callback",
                "path",
            ),
            (
                "OAUTH_DASHBOARD_URL",
                "https://user@example.test/dashboard.html",
                "credentials",
            ),
            (
                "OAUTH_DASHBOARD_URL",
                "https://console.example.test/dashboard.html#other",
                "fragment",
            ),
        ] {
            let redirect = if name == "GITHUB_REDIRECT_URI" {
                value
            } else {
                "https://api.example.test/auth/github/callback"
            };
            let dashboard = if name == "OAUTH_DASHBOARD_URL" {
                Some(value)
            } else {
                Some("https://console.example.test/dashboard.html")
            };
            with_env_vars(
                vec![
                    ("GITHUB_CLIENT_ID", Some("cid")),
                    ("GITHUB_CLIENT_SECRET", Some("csec")),
                    ("GITHUB_REDIRECT_URI", Some(redirect)),
                    ("OAUTH_DASHBOARD_URL", dashboard),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(error.contains(name), "unexpected error: {error}");
                    assert!(error.contains(expected), "unexpected error: {error}");
                },
            );
        }
    }

    #[test]
    fn test_local_registration_rejects_other_identity_sources() {
        with_env_vars(
            vec![
                ("BIND_ADDR", Some("127.0.0.1:3000")),
                ("GITHUB_CLIENT_ID", None),
                ("GITHUB_CLIENT_SECRET", None),
                ("STATIC_TOKENS", Some("ops:secret")),
                ("STATIC_TOKENS_FILE", None),
                ("LOCAL_REGISTRATION", Some("true")),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("cannot be combined"));
            },
        );

        with_env_vars(
            vec![
                ("BIND_ADDR", Some("127.0.0.1:3000")),
                ("GITHUB_CLIENT_ID", Some("client")),
                ("GITHUB_CLIENT_SECRET", Some("secret")),
                (
                    "GITHUB_REDIRECT_URI",
                    Some("https://api.octostore.io/auth/github/callback"),
                ),
                ("STATIC_TOKENS", None),
                ("STATIC_TOKENS_FILE", None),
                ("LOCAL_REGISTRATION", Some("true")),
            ],
            || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("cannot be combined"));
            },
        );
    }

    #[test]
    fn test_config_rejects_malformed_public_elections_boolean() {
        with_env_vars(vec![("PUBLIC_ELECTIONS", Some("flase"))], || {
            let error = Config::from_env().unwrap_err().to_string();
            assert!(error.contains("PUBLIC_ELECTIONS"));
        });
    }

    #[test]
    fn test_config_rejects_zero_or_invalid_security_limits() {
        for (name, value) in [
            ("MAX_PUBLIC_ELECTIONS", "0"),
            ("PUBLIC_ELECTION_REQUESTS_PER_MINUTE", "not-a-number"),
            ("PUBLIC_ELECTION_WATCH_STREAMS_GLOBAL", "0"),
            ("PUBLIC_ELECTION_WATCH_STREAMS_PER_CLIENT", "-1"),
            ("PUBLIC_ELECTION_WATCH_MAX_SECONDS", "invalid"),
        ] {
            with_env_vars(vec![(name, Some(value))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains(name), "unexpected error for {name}: {error}");
            });
        }
    }

    #[test]
    fn test_config_rejects_unreadable_static_tokens_file() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.tokens");
        let missing = missing.to_str().unwrap();
        with_env_vars(vec![("STATIC_TOKENS_FILE", Some(missing))], || {
            let error = Config::from_env().unwrap_err().to_string();
            assert!(error.contains("STATIC_TOKENS_FILE"));
        });
    }

    #[test]
    fn test_config_rejects_malformed_static_tokens_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "alice:").unwrap();
        let path = file.path().to_str().unwrap();
        with_env_vars(vec![("STATIC_TOKENS_FILE", Some(path))], || {
            let error = Config::from_env().unwrap_err().to_string();
            assert!(error.contains("empty token"));
            assert!(!error.contains("alice:"));
        });
    }

    #[test]
    fn test_config_rejects_empty_explicit_security_values() {
        for name in ["STATIC_TOKENS", "ADMIN_KEY", "ADMIN_USERNAME"] {
            with_env_vars(vec![(name, Some(""))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains(name));
            });
        }
    }
}
