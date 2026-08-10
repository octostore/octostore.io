mod app;
mod auth;
mod cli;
mod config;
mod elections;
mod error;
mod locks;
mod metrics;
mod models;
mod rate_limit;
mod sessions;
mod store;
mod webhooks;

use app::AppState;
use auth::{
    github_auth, github_callback, github_exchange, register_local, rotate_token, AuthService,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header::CONTENT_TYPE, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use clap::Parser;
use config::Config;
use elections::{
    campaign, create_election, election_status, renew_leadership, resign_leadership, watch_election,
};
use locks::{
    acquire_lock, get_lock_status, list_locks, release_lock, renew_lock, update_lock_acl,
    watch_lock, LockHandlers,
};
use metrics::{endpoint_from_path, Metrics};
use rate_limit::{PublicElectionRateLimiter, PublicElectionWatchLimiter};
use sessions::SessionStore;
use webhooks::{create_webhook_handler, delete_webhook_handler, list_webhooks, WebhookStore};

use std::sync::{Arc, Mutex};
use store::{DbConn, LockStore};
use tokio::signal;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn cors_layer(config: &Config) -> CorsLayer {
    let mut allowed_origins = vec![
        "https://octostore.io".parse().unwrap(),
        "https://www.octostore.io".parse().unwrap(),
        "http://localhost:3000".parse().unwrap(),
        "http://localhost:4173".parse().unwrap(),
        "http://127.0.0.1:4173".parse().unwrap(),
    ];
    if let Some(origin) = config.oauth_dashboard_origin() {
        let origin = origin
            .parse::<HeaderValue>()
            .expect("OAuth dashboard origin was validated at startup");
        if !allowed_origins.contains(&origin) {
            allowed_origins.push(origin);
        }
    }
    CorsLayer::new()
        .allow_origin(allowed_origins)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
        ])
        .max_age(std::time::Duration::from_secs(600))
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderName::from_static("x-admin-key"),
            axum::http::HeaderName::from_static("x-octostore-admin-key"),
        ])
        .expose_headers([
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderName::from_static(crate::error::REQUEST_ID_HEADER),
        ])
}

async fn request_id_middleware(request: Request, next: Next) -> Response {
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    crate::error::with_request_id(request_id.clone(), async move {
        let response = next.run(request).await;
        let mut response = normalize_framework_error(response, &request_id).await;
        if let Ok(value) = HeaderValue::from_str(&request_id) {
            response
                .headers_mut()
                .insert(crate::error::REQUEST_ID_HEADER, value);
        }
        response
    })
    .await
}

async fn normalize_framework_error(response: Response, request_id: &str) -> Response {
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if is_json {
        return response;
    }

    let status = response.status();
    let (error, code, details) = match status {
        StatusCode::BAD_REQUEST
        | StatusCode::PAYLOAD_TOO_LARGE
        | StatusCode::UNSUPPORTED_MEDIA_TYPE
        | StatusCode::UNPROCESSABLE_ENTITY
        | StatusCode::METHOD_NOT_ALLOWED => (
            "Invalid input",
            "invalid_input",
            "The request could not be parsed or is not supported by this endpoint",
        ),
        StatusCode::NOT_FOUND => ("Resource not found", "not_found", "Resource not found"),
        StatusCode::CONFLICT => ("Conflict", "conflict", "Conflict"),
        _ if status.is_server_error() => (
            "Internal server error",
            "internal_error",
            "Internal server error",
        ),
        _ => ("Request failed", "invalid_input", "Request failed"),
    };

    let (mut parts, _body) = response.into_parts();
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let body = serde_json::json!({
        "error": error,
        "code": code,
        "details": details,
        "request_id": request_id,
    });
    Response::from_parts(
        parts,
        Body::from(serde_json::to_vec(&body).unwrap_or_default()),
    )
}

static VERSIONED_SPEC: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn versioned_openapi_spec() -> &'static str {
    VERSIONED_SPEC.get_or_init(|| {
        include_str!("../openapi.yaml").replacen(
            "  version: 0.0.0-dev",
            &format!("  version: {}", env!("CARGO_PKG_VERSION")),
            1,
        )
    })
}

/// Validates that the request carries a valid admin credential.
///
/// Accepts either an `X-Admin-Key` / `X-OctoStore-Admin-Key` header,
/// a `Bearer admin:<key>` authorization header, or a regular bearer token
/// belonging to the configured username with durable OAuth provenance.
fn require_admin(headers: &axum::http::HeaderMap, state: &AppState) -> crate::error::Result<()> {
    let provided_key = headers
        .get("x-admin-key")
        .or_else(|| headers.get("x-octostore-admin-key"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer admin:"))
                .map(|s| s.to_string())
        });

    let admin_key_valid = match (&provided_key, &state.config.admin_key) {
        (Some(provided), Some(expected)) => provided == expected,
        _ => false,
    };

    if admin_key_valid {
        return Ok(());
    }

    // Fall back to OAuth-based admin check
    let user_id = state.auth_service.authenticate(headers)?;

    let Some(admin_username) = state.config.admin_username.as_deref() else {
        return Err(crate::error::AppError::Forbidden(
            "Admin access required".to_string(),
        ));
    };
    if state.auth_service.is_oauth_user(user_id, admin_username)? {
        Ok(())
    } else {
        Err(crate::error::AppError::Forbidden(
            "Admin access required".to_string(),
        ))
    }
}

// Handler to serve OpenAPI spec
async fn openapi_spec() -> impl IntoResponse {
    let mut response = Response::new(versioned_openapi_spec().to_string());
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/yaml"));
    response
}

// Serve a deliberately non-interactive API index. The machine-readable OpenAPI
// document is the contract; this page never executes third-party code or accepts
// credentials in a browser-owned API console.
async fn api_docs() -> Response {
    let html_content = r#"<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>OctoStore API Documentation</title>
    <style>
        :root { color-scheme: dark; font-family: ui-sans-serif, system-ui, sans-serif; }
        body { max-width: 48rem; margin: 10vh auto; padding: 0 1.5rem; background: #09090b; color: #e4e4e7; line-height: 1.6; }
        h1 { color: #fff; letter-spacing: -0.03em; }
        a { color: #a5b4fc; }
        code { padding: .15rem .35rem; border-radius: .3rem; background: #18181b; }
        .note { padding: 1rem; border: 1px solid #3f3f46; border-radius: .6rem; }
    </style>
</head>
<body>
    <main>
        <h1>OctoStore API</h1>
        <p>Stop two agents from doing the same work. Start with the
            <a href="https://octostore.io/agents/SKILL.md">agent skill</a>, then use the CLI.</p>
        <p>The complete, versioned machine contract is
            <a href="/openapi.yaml"><code>/openapi.yaml</code></a>.</p>
        <p class="note">This documentation is intentionally read-only. It does not load runtime
            JavaScript, collect a bearer token, or make authenticated requests from your browser.</p>
    </main>
</body>
</html>"#;

    let mut response = Html(html_content).into_response();
    response.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

// Metrics middleware to track request latencies
async fn metrics_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let start = std::time::Instant::now();
    let path = request.uri().path().to_string();

    let response = next.run(request).await;

    let duration = start.elapsed();
    let duration_ms = duration.as_micros() as f64 / 1000.0;

    // Determine if this was an error (4xx/5xx status codes)
    let is_error = response.status().as_u16() >= 400;

    // Map path to endpoint name
    if let Some(endpoint) = endpoint_from_path(&path) {
        state
            .metrics
            .record_request(endpoint, duration_ms, is_error);
    }

    response
}

// Metrics endpoint - requires admin key
async fn metrics_endpoint(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> crate::error::Result<axum::Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    // Get metrics snapshot
    let mut metrics_json = state.metrics.snapshot();

    // Add current active locks and users count
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    let users = state.auth_service.get_all_users().unwrap_or_default();

    // Update the snapshot with real data
    if let Some(obj) = metrics_json.as_object_mut() {
        obj.insert(
            "active_locks".to_string(),
            serde_json::Value::Number(active_locks.len().into()),
        );
        obj.insert(
            "total_users".to_string(),
            serde_json::Value::Number(users.len().into()),
        );
    }

    Ok(axum::Json(metrics_json))
}

// Timeseries metrics endpoint - requires admin key
async fn timeseries_endpoint(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> crate::error::Result<axum::Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    // Keep the implementation and documented enum exact. Returning one-hour
    // data under an arbitrary caller-provided label would make the response
    // actively misleading to operators and agents.
    let window = params.get("window").map(|s| s.as_str()).unwrap_or("1h");
    if !matches!(window, "1h" | "12h" | "24h" | "7d") {
        return Err(crate::error::AppError::InvalidInput(
            "window must be one of: 1h, 12h, 24h, 7d".to_string(),
        ));
    }

    // Update active locks count in time series before returning data
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    state
        .metrics
        .update_active_locks_count(active_locks.len() as u64);

    // Get time series data
    let timeseries_data = state.metrics.get_timeseries_data(window);

    Ok(axum::Json(timeseries_data))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let parsed = match cli::Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(error) => {
            let exit_code = match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => cli::EXIT_USAGE,
            };
            error.print()?;
            std::process::exit(exit_code);
        }
    };
    match parsed.command {
        None | Some(cli::Command::Serve) => {}
        Some(command) => std::process::exit(cli::run(command).await),
    }

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "octostore_lock=debug,tower_http=debug,axum::rejection=trace".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration
    let config = Config::from_env()?;
    info!("Starting octostore-lock on {}", config.bind_addr);

    // Open one SQLite connection shared by both AuthService and LockStore (#19)
    let db: DbConn = Arc::new(Mutex::new(rusqlite::Connection::open(
        &config.database_url,
    )?));

    // Initialize auth service
    let auth_service = AuthService::new(config.clone(), db.clone())?;

    // Seed static tokens (no-op when GitHub OAuth is enabled)
    if !config.is_github_enabled() {
        auth_service.seed_static_tokens()?;
        if config.local_registration_enabled {
            tracing::warn!(
                "LOCAL_REGISTRATION is enabled on loopback; disable it after enrolling the required local users"
            );
        } else if config.static_tokens.is_none() && config.static_tokens_file.is_none() {
            tracing::warn!(
                "No authenticated identity source is configured; lock and session APIs will reject all callers"
            );
            tracing::warn!(
                "Set STATIC_TOKENS or STATIC_TOKENS_FILE, or use loopback-only LOCAL_REGISTRATION=true for explicit bootstrap"
            );
        }
    }

    // Load fencing counter from database
    let initial_fencing_token = auth_service.load_fencing_counter()?;
    info!("Loaded fencing counter: {}", initial_fencing_token);

    // Initialize lock store — reuses the same shared DbConn as AuthService
    let lock_store = LockStore::new(db.clone(), initial_fencing_token)?;

    // Initialize session store — reuses the same shared DbConn
    let session_store = SessionStore::new(db.clone())?;
    session_store.reconcile_ephemeral_locks(&lock_store);

    // Initialize webhook store — reuses the same shared DbConn
    let webhook_store = WebhookStore::new(db)?;
    lock_store.set_webhook_store(webhook_store.clone());

    // Start background expiry tasks
    let expiry_store = lock_store.clone();
    expiry_store.start_expiry_task();

    let session_expiry_store = session_store.clone();
    session_expiry_store.start_expiry_task(lock_store.clone());

    // Create app state
    let lock_handlers = LockHandlers::new(lock_store.clone());
    let metrics = Metrics::new();
    let app_state = AppState {
        lock_handlers: lock_handlers.clone(),
        auth_service: auth_service.clone(),
        config: config.clone(),
        metrics: metrics.clone(),
        public_election_rate_limiter: PublicElectionRateLimiter::new(
            config.public_election_requests_per_minute,
        ),
        public_election_watch_limiter: PublicElectionWatchLimiter::new(
            config.public_election_watch_streams_global,
            config.public_election_watch_streams_per_client,
        ),
        session_store: session_store.clone(),
        webhook_store: webhook_store.clone(),
    };

    // Build only the explicitly configured enrollment surface. Local
    // registration is fail-closed by default and config validation constrains
    // it to an explicit numeric loopback bind address.
    let auth_router = if config.is_github_enabled() {
        Router::new()
            .route("/auth/github", get(github_auth))
            .route("/auth/github/callback", get(github_callback))
            .route("/auth/github/exchange", post(github_exchange))
    } else if config.local_registration_enabled {
        Router::new().route("/auth/register", post(register_local))
    } else {
        Router::new()
    };

    let app = Router::new()
        .merge(auth_router)
        // Auth routes (always available)
        .route("/auth/token/rotate", post(rotate_token))
        // Session routes
        .route("/sessions", post(sessions::create_session))
        .route("/sessions/:id/keepalive", post(sessions::keepalive))
        .route(
            "/sessions/:id",
            get(sessions::get_session_status).delete(sessions::terminate_session),
        )
        // Lock routes
        .route("/locks/:name/acquire", post(acquire_lock))
        .route("/locks/:name/acl", put(update_lock_acl))
        .route("/locks/:name/release", post(release_lock))
        .route("/locks/:name/renew", post(renew_lock))
        .route("/locks/:name/watch", get(watch_lock))
        .route("/locks/:name", get(get_lock_status))
        .route("/locks", get(list_locks))
        // Public, account-free leader election routes
        .route("/elections", post(create_election))
        .route("/elections/:id", get(election_status))
        .route("/elections/:id/watch", get(watch_election))
        .route("/elections/:id/campaign", post(campaign))
        .route("/elections/:id/renew", post(renew_leadership))
        .route("/elections/:id/resign", post(resign_leadership))
        // Webhook routes
        .route("/webhooks", post(create_webhook_handler).get(list_webhooks))
        .route(
            "/webhooks/:id",
            axum::routing::delete(delete_webhook_handler),
        )
        // Documentation routes
        .route("/", get(api_docs))
        .route("/openapi.yaml", get(openapi_spec))
        .route("/docs", get(api_docs))
        // Health check
        .route("/health", get(health_check))
        .fallback(|| async { StatusCode::NOT_FOUND })
        // Public status endpoint (no auth)
        .route("/status", get(status_check))
        // Admin routes
        .route("/admin/status", get(admin_status))
        .route("/admin/metrics/timeseries", get(timeseries_endpoint))
        // Metrics endpoint
        .route("/metrics", get(metrics_endpoint))
        // Add metrics middleware layer
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            metrics_middleware,
        ))
        .layer(cors_layer(&config))
        // Request IDs are outermost so CORS preflight short-circuits still
        // receive the same correlation header as ordinary API responses.
        .layer(middleware::from_fn(request_id_middleware))
        // Add state
        .with_state(app_state);

    // Create listener
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    info!("Server listening on {}", config.bind_addr);

    // Set up graceful shutdown handling
    let shutdown_lock_store = lock_store.clone();
    let shutdown_auth_service = Arc::new(auth_service);

    // Start the server with graceful shutdown
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_lock_store, shutdown_auth_service))
    .await?;

    info!("Server stopped");
    Ok(())
}

async fn health_check(State(state): State<AppState>) -> Json<serde_json::Value> {
    let db_size_bytes = std::fs::metadata(&state.config.database_url)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    Json(serde_json::json!({
        "status": "ok",
        "storage": "wal",
        "db_size_bytes": db_size_bytes,
    }))
}

async fn status_check(State(state): State<AppState>) -> Json<serde_json::Value> {
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    let users = state.auth_service.get_all_users().unwrap_or_default();
    let uptime_seconds = state.metrics.start_time.elapsed().as_secs();
    let total_acquires = state
        .metrics
        .lock_store_acquires
        .load(std::sync::atomic::Ordering::Relaxed);
    let total_releases = state
        .metrics
        .lock_store_releases
        .load(std::sync::atomic::Ordering::Relaxed);

    Json(serde_json::json!({
        "status": "ok",
        "uptime_seconds": uptime_seconds,
        "active_locks": active_locks.len(),
        "total_users": users.len(),
        "total_acquires": total_acquires,
        "total_releases": total_releases
    }))
}

async fn admin_status(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> crate::error::Result<axum::Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    // Get all active locks
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    let locks: Vec<serde_json::Value> = active_locks
        .into_iter()
        .map(|lock| {
            // Get holder username
            let holder_username = state
                .auth_service
                .get_user_by_id(&lock.holder_id.to_string())
                .unwrap_or(None)
                .unwrap_or_else(|| "unknown".to_string());

            let now = chrono::Utc::now();
            let ttl_remaining = if lock.expires_at > now {
                (lock.expires_at - now).num_seconds()
            } else {
                0
            };

            serde_json::json!({
                "name": lock.name,
                "holder_username": holder_username,
                "metadata": lock.metadata,
                "fencing_token": lock.fencing_token,
                "expires_at": lock.expires_at.to_rfc3339(),
                "ttl_remaining_seconds": ttl_remaining
            })
        })
        .collect();

    // Get all registered users
    let users = state.auth_service.get_all_users().unwrap_or_default();

    let uptime_seconds = state.metrics.start_time.elapsed().as_secs();
    let total_acquires = state
        .metrics
        .lock_store_acquires
        .load(std::sync::atomic::Ordering::Relaxed);
    let total_releases = state
        .metrics
        .lock_store_releases
        .load(std::sync::atomic::Ordering::Relaxed);
    let active_locks = locks.len();
    let total_users = users.len();

    let response = serde_json::json!({
        "healthy": true,
        "uptime_seconds": uptime_seconds,
        "active_locks": active_locks,
        "total_users": total_users,
        "total_acquires": total_acquires,
        "total_releases": total_releases,
        "locks": locks,
        "users": users
    });

    Ok(axum::Json(response))
}

async fn shutdown_signal(lock_store: LockStore, auth_service: Arc<AuthService>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, starting graceful shutdown");
        },
        _ = terminate => {
            info!("Received SIGTERM, starting graceful shutdown");
        }
    }

    // Save fencing counter to database before shutdown
    let fencing_counter = lock_store.get_fencing_counter();
    if let Err(e) = auth_service.save_fencing_counter(fencing_counter) {
        warn!("Failed to save fencing counter: {}", e);
    } else {
        info!("Saved fencing counter: {}", fencing_counter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tempfile::NamedTempFile;
    use tower::util::ServiceExt; // for oneshot

    fn oauth_test_config(database_url: String) -> Config {
        Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url,
            github_client_id: Some("test_client_id".to_string()),
            github_client_secret: Some("test_client_secret".to_string()),
            github_redirect_uri: "http://localhost:3000/auth/github/callback".to_string(),
            oauth_api_base_url: Some("http://localhost:3000".to_string()),
            oauth_dashboard_url: Some("http://localhost:4173/dashboard.html".to_string()),
            admin_key: Some("test_admin_key".to_string()),
            admin_username: None,
            static_tokens: None,
            static_tokens_file: None,
            local_registration_enabled: false,
            public_elections_enabled: true,
            max_public_elections: 100,
            public_election_requests_per_minute: 600,
            public_election_watch_streams_global: 100,
            public_election_watch_streams_per_client: 8,
            public_election_watch_max_seconds: 900,
        }
    }

    fn test_app_state(config: Config) -> AppState {
        let db: DbConn = Arc::new(Mutex::new(
            rusqlite::Connection::open(&config.database_url).unwrap(),
        ));
        let auth_service = AuthService::new(config.clone(), db.clone()).unwrap();
        let lock_store = LockStore::new(db.clone(), 0).unwrap();
        let session_store = SessionStore::new(db.clone()).unwrap();
        let webhook_store = WebhookStore::new(db).unwrap();

        AppState {
            lock_handlers: LockHandlers::new(lock_store),
            auth_service,
            config: config.clone(),
            metrics: Metrics::new(),
            public_election_rate_limiter: PublicElectionRateLimiter::new(
                config.public_election_requests_per_minute,
            ),
            public_election_watch_limiter: PublicElectionWatchLimiter::new(
                config.public_election_watch_streams_global,
                config.public_election_watch_streams_per_client,
            ),
            session_store,
            webhook_store,
        }
    }

    async fn create_test_app() -> Router {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        let config = oauth_test_config(db_path);
        let app_state = test_app_state(config.clone());

        let auth_router = if config.is_github_enabled() {
            Router::new()
                .route("/auth/github", get(github_auth))
                .route("/auth/github/callback", get(github_callback))
                .route("/auth/github/exchange", post(github_exchange))
        } else if config.local_registration_enabled {
            Router::new().route("/auth/register", post(register_local))
        } else {
            Router::new()
        };

        Router::new()
            .merge(auth_router)
            .route("/auth/token/rotate", post(rotate_token))
            .route("/locks/:name/acquire", post(acquire_lock))
            .route("/locks/:name/acl", put(update_lock_acl))
            .route("/locks/:name/release", post(release_lock))
            .route("/locks/:name/renew", post(renew_lock))
            .route("/locks/:name/watch", get(watch_lock))
            .route("/locks/:name", get(get_lock_status))
            .route("/locks", get(list_locks))
            .route("/elections", post(create_election))
            .route("/elections/:id", get(election_status))
            .route("/elections/:id/watch", get(watch_election))
            .route("/elections/:id/campaign", post(campaign))
            .route("/elections/:id/renew", post(renew_leadership))
            .route("/elections/:id/resign", post(resign_leadership))
            .route("/openapi.yaml", get(openapi_spec))
            .route("/docs", get(api_docs))
            .route("/health", get(health_check))
            .route("/status", get(status_check))
            .route("/admin/status", get(admin_status))
            .route("/admin/metrics/timeseries", get(timeseries_endpoint))
            .route("/metrics", get(metrics_endpoint))
            .layer(middleware::from_fn_with_state(
                app_state.clone(),
                metrics_middleware,
            ))
            .layer(cors_layer(&config))
            .layer(middleware::from_fn(request_id_middleware))
            .with_state(app_state)
    }

    fn create_oauth_flow_app(
        config: Config,
        token_endpoint: String,
        user_endpoint: String,
    ) -> Router {
        let db: DbConn = Arc::new(Mutex::new(
            rusqlite::Connection::open(&config.database_url).unwrap(),
        ));
        let auth_service = AuthService::new(config.clone(), db.clone())
            .unwrap()
            .with_github_endpoints(token_endpoint, user_endpoint);
        let lock_store = LockStore::new(db.clone(), 0).unwrap();
        let session_store = SessionStore::new(db.clone()).unwrap();
        let webhook_store = WebhookStore::new(db).unwrap();
        let app_state = AppState {
            lock_handlers: LockHandlers::new(lock_store),
            auth_service,
            config: config.clone(),
            metrics: Metrics::new(),
            public_election_rate_limiter: PublicElectionRateLimiter::new(600),
            public_election_watch_limiter: PublicElectionWatchLimiter::new(100, 8),
            session_store,
            webhook_store,
        };

        Router::new()
            .route("/auth/github", get(github_auth))
            .route("/auth/github/callback", get(github_callback))
            .route("/auth/github/exchange", post(github_exchange))
            .layer(cors_layer(&config))
            .layer(middleware::from_fn(request_id_middleware))
            .with_state(app_state)
    }

    #[tokio::test]
    async fn test_health_check_reports_wal_storage_details() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response
            .headers()
            .get(crate::error::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok());
        assert!(request_id.is_some_and(|value| value.starts_with("req_")));

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["status"], "ok");
        assert_eq!(body_json["storage"], "wal");
        assert!(body_json["db_size_bytes"].as_u64().is_some());
    }

    #[tokio::test]
    async fn test_openapi_spec() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.yaml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/yaml"
        );
    }

    #[test]
    fn openapi_covers_every_public_route_and_stable_error_code() {
        let document: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../openapi.yaml")).expect("OpenAPI must be YAML");
        let paths = document["paths"]
            .as_mapping()
            .expect("OpenAPI paths must be a mapping");
        let expected_paths = [
            "/",
            "/docs",
            "/openapi.yaml",
            "/health",
            "/status",
            "/admin/status",
            "/metrics",
            "/admin/metrics/timeseries",
            "/auth/github",
            "/auth/github/callback",
            "/auth/github/exchange",
            "/auth/register",
            "/auth/token/rotate",
            "/sessions",
            "/sessions/{id}",
            "/sessions/{id}/keepalive",
            "/locks",
            "/locks/{name}",
            "/locks/{name}/acquire",
            "/locks/{name}/acl",
            "/locks/{name}/release",
            "/locks/{name}/renew",
            "/locks/{name}/watch",
            "/elections",
            "/elections/{election_id}",
            "/elections/{election_id}/campaign",
            "/elections/{election_id}/renew",
            "/elections/{election_id}/resign",
            "/elections/{election_id}/watch",
            "/webhooks",
            "/webhooks/{id}",
        ];
        for path in expected_paths {
            assert!(
                paths.contains_key(serde_yaml::Value::String(path.to_string())),
                "OpenAPI is missing {path}"
            );
        }

        let expected_operations = [
            ("/", "get"),
            ("/docs", "get"),
            ("/openapi.yaml", "get"),
            ("/health", "get"),
            ("/status", "get"),
            ("/admin/status", "get"),
            ("/metrics", "get"),
            ("/admin/metrics/timeseries", "get"),
            ("/auth/github", "get"),
            ("/auth/github/callback", "get"),
            ("/auth/github/exchange", "post"),
            ("/auth/register", "post"),
            ("/auth/token/rotate", "post"),
            ("/sessions", "post"),
            ("/sessions/{id}", "get"),
            ("/sessions/{id}", "delete"),
            ("/sessions/{id}/keepalive", "post"),
            ("/locks", "get"),
            ("/locks/{name}", "get"),
            ("/locks/{name}/acquire", "post"),
            ("/locks/{name}/acl", "put"),
            ("/locks/{name}/release", "post"),
            ("/locks/{name}/renew", "post"),
            ("/locks/{name}/watch", "get"),
            ("/elections", "post"),
            ("/elections/{election_id}", "get"),
            ("/elections/{election_id}/campaign", "post"),
            ("/elections/{election_id}/renew", "post"),
            ("/elections/{election_id}/resign", "post"),
            ("/elections/{election_id}/watch", "get"),
            ("/webhooks", "get"),
            ("/webhooks", "post"),
            ("/webhooks/{id}", "delete"),
        ];
        for (path, method) in expected_operations {
            assert!(
                document["paths"][path][method].is_mapping(),
                "OpenAPI is missing {method} {path}"
            );
        }

        let code_values = document["components"]["schemas"]["Error"]["properties"]["code"]["enum"]
            .as_sequence()
            .expect("Error.code must have a finite enum")
            .iter()
            .filter_map(serde_yaml::Value::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let expected_codes = [
            "authentication_required",
            "authentication_failed",
            "forbidden",
            "invalid_input",
            "invalid_ttl",
            "invalid_lock_name",
            "not_found",
            "session_expired",
            "lease_not_current",
            "conflict",
            "capacity_exceeded",
            "lock_limit_exceeded",
            "rate_limited",
            "upstream_unavailable",
            "internal_error",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(code_values, expected_codes);

        for (path, schema) in [
            ("/admin/status", "AdminStatus"),
            ("/metrics", "AdminMetrics"),
            ("/admin/metrics/timeseries", "AdminMetricsTimeseries"),
        ] {
            assert_eq!(
                document["paths"][path]["get"]["responses"]["200"]["content"]["application/json"]
                    ["schema"]["$ref"]
                    .as_str(),
                Some(format!("#/components/schemas/{schema}").as_str()),
                "{path} must use an exact named response schema"
            );
            assert_eq!(
                document["components"]["schemas"][schema]["additionalProperties"].as_bool(),
                Some(false),
                "{schema} must reject undocumented response fields"
            );
        }
        let windows = document["paths"]["/admin/metrics/timeseries"]["get"]["parameters"][0]
            ["schema"]["enum"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(serde_yaml::Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(windows, vec!["1h", "12h", "24h", "7d"]);
        assert_eq!(
            document["paths"]["/admin/metrics/timeseries"]["get"]["responses"]["400"]["$ref"]
                .as_str(),
            Some("#/components/responses/ValidationError")
        );
        assert_eq!(
            document["components"]["schemas"]["WebhookEvent"]["additionalProperties"].as_bool(),
            Some(false)
        );

        let create_required = document["paths"]["/elections"]["post"]["responses"]["201"]
            ["content"]["application/json"]["schema"]["required"]
            .as_sequence()
            .unwrap();
        assert!(create_required
            .iter()
            .any(|field| field.as_str() == Some("watch_path")));

        for (path, status, expected_ref) in [
            (
                "/locks/{name}/release",
                "400",
                "#/components/responses/LeaseNotCurrent",
            ),
            (
                "/locks/{name}/release",
                "404",
                "#/components/responses/LeaseNotCurrent",
            ),
            (
                "/locks/{name}/renew",
                "400",
                "#/components/responses/LeaseNotCurrent",
            ),
            (
                "/locks/{name}/renew",
                "404",
                "#/components/responses/LeaseNotCurrent",
            ),
            (
                "/locks/{name}/watch",
                "409",
                "#/components/responses/CapacityExceeded",
            ),
        ] {
            assert_eq!(
                document["paths"][path]["get"]
                    .as_mapping()
                    .and_then(
                        |_| document["paths"][path]["get"]["responses"][status]["$ref"].as_str()
                    )
                    .or_else(
                        || document["paths"][path]["post"]["responses"][status]["$ref"].as_str()
                    ),
                Some(expected_ref),
                "unexpected response contract for {path} HTTP {status}"
            );
        }

        let lock_watch_example = serde_yaml::to_string(
            &document["paths"]["/locks/{name}/watch"]["get"]["responses"]["200"]["content"]
                ["text/event-stream"]["example"],
        )
        .expect("lock watch example must serialize");
        for forbidden in ["lease_id", "session_id", "holder_id", "metadata"] {
            assert!(
                !lock_watch_example.contains(forbidden),
                "lock watch example leaked {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn test_api_docs() {
        let app = create_test_app().await;

        let response = app
            .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("OctoStore API Documentation"));
        assert!(body_str.contains("/openapi.yaml"));
        assert!(body_str.contains("intentionally read-only"));
        assert!(!body_str.contains("<script"));
        assert!(!body_str.contains("cdn.jsdelivr.net"));
    }

    #[tokio::test]
    async fn test_api_docs_send_a_restrictive_http_csp() {
        let app = create_test_app().await;

        let response = app
            .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("object-src 'none'"));
        assert!(csp.contains("base-uri 'none'"));
        assert!(csp.contains("form-action 'none'"));
        assert!(!csp.contains("script-src"));
        assert_eq!(
            response
                .headers()
                .get("x-content-type-options")
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
    }

    #[tokio::test]
    async fn test_github_auth_redirect() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/auth/github")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let location = response.headers().get("location").unwrap();
        let location_str = location.to_str().unwrap();
        assert!(location_str.contains("https://github.com/login/oauth/authorize"));
        assert!(location_str.contains("client_id=test_client_id"));
        assert!(location_str.contains("state="));
        assert!(response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.contains("__Host-octostore-oauth-state=")
                    && value.contains("HttpOnly")
                    && value.contains("Secure")
                    && value.contains("SameSite=Lax")
            }));
    }

    #[tokio::test]
    async fn self_hosted_oauth_http_flow_keeps_dashboard_bound_to_its_issuer() {
        let github_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let github_address = github_listener.local_addr().unwrap();
        let github_app = Router::new()
            .route(
                "/login/oauth/access_token",
                post(|| async { Json(json!({ "access_token": "fixture-upstream-token" })) }),
            )
            .route(
                "/user",
                get(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(
                        headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok()),
                        Some("Bearer fixture-upstream-token")
                    );
                    Json(json!({ "id": 424242, "login": "self-hosted-user" }))
                }),
            );
        let github_task = tokio::spawn(async move {
            axum::serve(github_listener, github_app).await.unwrap();
        });

        let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_address = api_listener.local_addr().unwrap();
        let api_base = format!("http://{api_address}");
        let dashboard_url = "http://localhost:4173/dashboard.html";
        let database = NamedTempFile::new().unwrap();
        let config = Config {
            bind_addr: api_address.to_string(),
            database_url: database.path().to_string_lossy().into_owned(),
            github_client_id: Some("self-hosted-client".to_string()),
            github_client_secret: Some("self-hosted-secret".to_string()),
            github_redirect_uri: format!("{api_base}/auth/github/callback"),
            oauth_api_base_url: Some(api_base.clone()),
            oauth_dashboard_url: Some(dashboard_url.to_string()),
            admin_key: None,
            admin_username: None,
            static_tokens: None,
            static_tokens_file: None,
            local_registration_enabled: false,
            public_elections_enabled: true,
            max_public_elections: 100,
            public_election_requests_per_minute: 600,
            public_election_watch_streams_global: 100,
            public_election_watch_streams_per_client: 8,
            public_election_watch_max_seconds: 900,
        };
        let app = create_oauth_flow_app(
            config,
            format!("http://{github_address}/login/oauth/access_token"),
            format!("http://{github_address}/user"),
        );
        let api_task = tokio::spawn(async move {
            axum::serve(api_listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let begin = client
            .get(format!("{api_base}/auth/github"))
            .send()
            .await
            .unwrap();
        assert_eq!(begin.status(), reqwest::StatusCode::SEE_OTHER);
        let authorize = url::Url::parse(
            begin
                .headers()
                .get(reqwest::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let state = authorize
            .query_pairs()
            .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
            .unwrap();
        let cookie = begin
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let callback = client
            .get(format!("{api_base}/auth/github/callback"))
            .query(&[("code", "fixture-code"), ("state", state.as_str())])
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(callback.status(), reqwest::StatusCode::SEE_OTHER);
        let dashboard = url::Url::parse(
            callback
                .headers()
                .get(reqwest::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            dashboard[..url::Position::AfterPath].trim_end_matches('/'),
            dashboard_url
        );
        let fragment = url::form_urlencoded::parse(dashboard.fragment().unwrap().as_bytes())
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fragment.get("issuer"), Some(&api_base));
        let exchange_code = fragment.get("exchange_code").unwrap();

        let preflight = client
            .request(
                reqwest::Method::OPTIONS,
                format!("{api_base}/auth/github/exchange"),
            )
            .header(reqwest::header::ORIGIN, "http://localhost:4173")
            .header(reqwest::header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .send()
            .await
            .unwrap();
        assert!(preflight.status().is_success());
        assert_eq!(
            preflight
                .headers()
                .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("http://localhost:4173")
        );

        let exchange_url = format!("{api_base}/auth/github/exchange");
        let exchanged = client
            .post(&exchange_url)
            .json(&json!({ "exchange_code": exchange_code }))
            .send()
            .await
            .unwrap();
        assert_eq!(exchanged.status(), reqwest::StatusCode::OK);
        let credential: Value = exchanged.json().await.unwrap();
        assert_eq!(credential["github_username"], "self-hosted-user");
        assert!(credential["token"]
            .as_str()
            .is_some_and(|token| !token.is_empty()));

        let reused = client
            .post(&exchange_url)
            .json(&json!({ "exchange_code": exchange_code }))
            .send()
            .await
            .unwrap();
        assert_eq!(reused.status(), reqwest::StatusCode::BAD_REQUEST);
        let reuse_error: Value = reused.json().await.unwrap();
        assert_eq!(reuse_error["code"], "invalid_input");

        api_task.abort();
        github_task.abort();
        let _ = api_task.await;
        let _ = github_task.await;
    }

    #[tokio::test]
    async fn test_authentication_required() {
        let app = create_test_app().await;

        // Try to access a protected endpoint without auth
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["error"], "Authorization header required");
    }

    #[tokio::test]
    async fn test_invalid_token() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks")
                    .header("authorization", "Bearer invalid_token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["error"], "Authentication failed");
    }

    #[tokio::test]
    async fn test_admin_status_unauthorized() {
        let app = create_test_app().await;

        // Try without any auth
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Try with wrong admin key
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .header("x-admin-key", "wrong_key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_admin_status_authorized() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .header("x-admin-key", "test_admin_key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(body_json["healthy"], true);
        assert!(body_json["uptime_seconds"].is_number());
        assert!(body_json["active_locks"].is_number());
        assert!(body_json["total_users"].is_number());
        assert!(body_json["locks"].is_array());
        assert!(body_json["users"].is_array());
    }

    #[tokio::test]
    async fn admin_username_requires_durable_oauth_identity_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = directory
            .path()
            .join("admin-provenance.db")
            .to_string_lossy()
            .into_owned();

        let mut local = oauth_test_config(database_url.clone());
        local.github_client_id = None;
        local.github_client_secret = None;
        local.oauth_api_base_url = None;
        local.oauth_dashboard_url = None;
        local.admin_key = None;
        local.static_tokens = Some("octoadmin:stale-static-token".to_string());
        let local_state = test_app_state(local);
        local_state.auth_service.seed_static_tokens().unwrap();
        drop(local_state);

        let mut oauth = oauth_test_config(database_url);
        oauth.admin_key = None;
        oauth.admin_username = Some("octoadmin".to_string());
        let state = test_app_state(oauth);
        let stale_headers = axum::http::HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer stale-static-token"),
        )]);
        assert!(matches!(
            require_admin(&stale_headers, &state),
            Err(crate::error::AppError::Unauthorized)
        ));

        let oauth_user = state
            .auth_service
            .create_or_get_user(crate::models::GitHubUser {
                id: 867_5309,
                login: "OctoAdmin".to_string(),
            })
            .await
            .unwrap();
        let oauth_headers = axum::http::HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", oauth_user.token)).unwrap(),
        )]);
        require_admin(&oauth_headers, &state).unwrap();
        assert!(matches!(
            require_admin(&stale_headers, &state),
            Err(crate::error::AppError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .header("x-admin-key", "test_admin_key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();

        assert!(body_json["uptime_seconds"].is_number());
        assert!(body_json["total_requests"].is_number());
        assert!(body_json["requests_per_second"].is_number());
        assert!(body_json["endpoints"].is_object());
        assert!(body_json["lock_store"].is_object());
        assert!(body_json["memory_bytes"].is_number());
    }

    #[tokio::test]
    async fn metrics_timeseries_rejects_an_unsupported_window_without_relabeling_data() {
        let app = create_test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/metrics/timeseries?window=30d")
                    .header("x-admin-key", "test_admin_key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let request_id = response
            .headers()
            .get(crate::error::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "invalid_input");
        assert_eq!(body["request_id"], request_id);
        assert!(body["details"]
            .as_str()
            .is_some_and(|details| details.contains("1h, 12h, 24h, 7d")));
    }

    #[tokio::test]
    async fn test_invalid_json_body() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer invalid_token")
                    .header("content-type", "application/json")
                    .body(Body::from("invalid json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let request_id = response
            .headers()
            .get(crate::error::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["code"], "invalid_input");
        assert_eq!(body_json["request_id"], request_id);
        assert!(body_json["details"].is_string());
    }

    #[tokio::test]
    async fn test_lock_name_validation() {
        let app = create_test_app().await;

        // Test with invalid lock name (spaces)
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/invalid%20name/acquire")
                    .method("POST")
                    .header("authorization", "Bearer invalid_token") // Will fail auth first
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ttl_seconds":60}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should get 401 for auth failure (auth is checked before validation)
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_cors_headers() {
        let app = create_test_app().await;

        // Make an OPTIONS request to check CORS
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/elections")
                    .method("OPTIONS")
                    .header("origin", "https://octostore.io")
                    .header("access-control-request-method", "POST")
                    .header("access-control-request-headers", "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("https://octostore.io")
        );
        assert!(response
            .headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(',').any(|method| method.trim() == "POST")));
        let request_id = response
            .headers()
            .get(crate::error::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok());
        assert!(request_id.is_some_and(|value| {
            value.len() == 36
                && value.starts_with("req_")
                && value[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
        }));
    }

    #[tokio::test]
    async fn test_content_type_handling() {
        let app = create_test_app().await;

        // Test without content-type header (should still work for JSON endpoints)
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer invalid_token")
                    // No content-type header
                    .body(Body::from(r#"{"ttl_seconds":60}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Content-type validation happens before auth
        assert!(
            response.status() == StatusCode::UNAUTHORIZED
                || response.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn test_large_request_body() {
        let app = create_test_app().await;

        // Create a very large JSON payload
        let large_metadata = "x".repeat(200_000); // 200KB string
        let large_request = json!({
            "ttl_seconds": 60,
            "metadata": large_metadata
        });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/test-lock/acquire")
                    .method("POST")
                    .header("authorization", "Bearer invalid_token")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&large_request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should fail on auth first (or potentially request size limits)
        assert!(response.status().is_client_error());
    }

    #[tokio::test]
    async fn test_path_traversal_protection() {
        let app = create_test_app().await;

        // Test with path traversal attempt in lock name
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks/../../../etc/passwd/acquire")
                    .method("POST")
                    .header("authorization", "Bearer invalid_token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ttl_seconds":60}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Axum should handle path normalization, but auth will fail first
        assert!(response.status().is_client_error());
    }

    #[tokio::test]
    async fn test_method_not_allowed() {
        let app = create_test_app().await;

        // Try PATCH on an endpoint that doesn't support it
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .method("PATCH")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_empty_path_segments() {
        let app = create_test_app().await;

        // Test with empty path segments
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/locks//acquire")
                    .method("POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should either be not found or handle gracefully
        assert!(response.status().is_client_error());
    }

    #[tokio::test]
    async fn test_metrics_middleware_basic() {
        let app = create_test_app().await;

        // Make a request to trigger metrics recording
        let _response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Metrics should have been recorded
        // We can't easily verify this without access to the app state,
        // but the middleware should run without errors
    }
}
