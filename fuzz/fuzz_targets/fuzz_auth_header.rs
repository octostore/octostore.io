#![no_main]
use libfuzzer_sys::fuzz_target;
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::{header::AUTHORIZATION, HeaderMap, HeaderValue};
use octostore::auth::AuthService;
use octostore::config::Config;
use rusqlite::Connection;

static AUTH_SERVICE: OnceLock<AuthService> = OnceLock::new();

fn service() -> &'static AuthService {
    AUTH_SERVICE.get_or_init(|| {
        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: ":memory:".to_string(),
            github_client_id: None,
            github_client_secret: None,
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            oauth_api_base_url: None,
            oauth_dashboard_url: None,
            admin_key: None,
            admin_username: None,
            static_tokens: Some("fuzzuser:fuzztoken".to_string()),
            static_tokens_file: None,
            local_registration_enabled: false,
            public_elections_enabled: true,
            max_public_elections: 10_000,
            public_election_requests_per_minute: 600,
            public_election_watch_streams_global: 1_024,
            public_election_watch_streams_per_client: 8,
            public_election_watch_max_seconds: 900,
        };
        let db = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let svc = AuthService::new(config, db).unwrap();
        svc.seed_static_tokens().unwrap();
        svc
    })
}

fuzz_target!(|data: &[u8]| {
    if let Ok(header_str) = std::str::from_utf8(data) {
        if let Ok(val) = HeaderValue::from_str(header_str) {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, val);
            let _ = service().authenticate(&headers);
        }
    }
});
