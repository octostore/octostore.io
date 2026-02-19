#![no_main]
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use octostore::auth::AuthService;
use octostore::config::Config;

static AUTH_SERVICE: OnceLock<AuthService> = OnceLock::new();

fn service() -> &'static AuthService {
    AUTH_SERVICE.get_or_init(|| {
        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: ":memory:".to_string(),
            github_client_id: None,
            github_client_secret: None,
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: None,
            admin_username: None,
            static_tokens: Some("fuzzuser:fuzztoken".to_string()),
            static_tokens_file: None,
        };
        let svc = AuthService::new(config).unwrap();
        svc.seed_static_tokens();
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
