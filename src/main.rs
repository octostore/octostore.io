mod auth;
mod config;
mod configstore;
mod error;
mod flags;
mod locks;
mod metrics;
mod models;
mod ratelimit;
mod store;

use auth::{github_auth, github_callback, rotate_token, AuthService};
use axum::{
    extract::{Request, State},
    http::HeaderValue,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
    Router,
};
use config::Config;
use configstore::{
    delete_config, get_config, get_config_history, list_configs, set_config, ConfigStoreHandlers,
};
use flags::{delete_flag, get_flag, list_flags, set_flag, FeatureFlagHandlers};
use locks::{acquire_lock, get_lock_status, list_user_locks, release_lock, renew_lock, LockHandlers};
use metrics::{endpoint_from_path, Metrics};
use ratelimit::{
    check_rate_limit, get_rate_limit_status, list_rate_limits, reset_rate_limit, RateLimitHandlers,
};

#[derive(Clone)]
pub struct AppState {
    pub lock_handlers: LockHandlers,
    pub ratelimit_handlers: RateLimitHandlers,
    pub flags_handlers: FeatureFlagHandlers,
    pub configstore_handlers: ConfigStoreHandlers,
    pub auth_service: AuthService,
    pub config: config::Config,
    pub metrics: Arc<Metrics>,
}
use std::sync::Arc;
use store::LockStore;
use tokio::signal;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Include OpenAPI spec at compile time
const OPENAPI_SPEC: &str = include_str!("../openapi.yaml");

// Handler to serve OpenAPI spec
async fn openapi_spec() -> impl IntoResponse {
    let mut response = Response::new(OPENAPI_SPEC.to_string());
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/yaml"),
    );
    response
}

// Handler to serve Scalar API documentation
async fn api_docs() -> impl IntoResponse {
    let html_content = r#"<!DOCTYPE html>
<html>
<head>
    <title>OctoStore API Documentation</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
</head>
<body>
    <script
        id="api-reference"
        data-url="/openapi.yaml"
        data-configuration='{"theme":"purple"}'
        src="https://cdn.jsdelivr.net/npm/@scalar/api-reference">
    </script>
</body>
</html>"#;
    
    Html(html_content)
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
        state.metrics.record_request(endpoint, duration_ms, is_error);
    }
    
    response
}

// Metrics endpoint - requires admin key
async fn metrics_endpoint(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<axum::Json<serde_json::Value>, axum::response::Response> {
    // Check admin key from header
    let provided_key = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let admin_key_valid = match (&provided_key, &state.config.admin_key) {
        (Some(provided), Some(expected)) => provided == expected,
        _ => false,
    };

    if !admin_key_valid {
        return Err(axum::response::Response::builder()
            .status(401)
            .body("Unauthorized: Provide X-Admin-Key header".into())
            .unwrap());
    }

    // Get metrics snapshot
    let mut metrics_json = state.metrics.snapshot();
    
    // Add current active locks and users count
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    let users = state.auth_service.get_all_users().unwrap_or_default();
    
    // Update the snapshot with real data
    if let Some(obj) = metrics_json.as_object_mut() {
        obj.insert("active_locks".to_string(), serde_json::Value::Number(active_locks.len().into()));
        obj.insert("total_users".to_string(), serde_json::Value::Number(users.len().into()));
    }

    Ok(axum::Json(metrics_json))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    // Initialize auth service
    let auth_service = AuthService::new(config.clone())?;
    
    // Load fencing counter from database
    let initial_fencing_token = auth_service.load_fencing_counter()?;
    info!("Loaded fencing counter: {}", initial_fencing_token);

    // Initialize lock store
    let lock_store = LockStore::new(&config.database_url, initial_fencing_token)?;
    
    // Start background expiry task
    let expiry_store = lock_store.clone();
    expiry_store.start_expiry_task();

    // Create app state
    let lock_handlers = LockHandlers::new(lock_store.clone(), auth_service.clone());
    let ratelimit_handlers = RateLimitHandlers::new(auth_service.clone());
    let flags_handlers = FeatureFlagHandlers::new(auth_service.clone());
    let configstore_handlers = ConfigStoreHandlers::new(auth_service.clone());
    let metrics = Metrics::new();
    let app_state = AppState {
        lock_handlers: lock_handlers.clone(),
        ratelimit_handlers: ratelimit_handlers.clone(),
        flags_handlers: flags_handlers.clone(),
        configstore_handlers: configstore_handlers.clone(),
        auth_service: auth_service.clone(),
        config: config.clone(),
        metrics: metrics.clone(),
    };

    // Build router
    let app = Router::new()
        // Auth routes
        .route("/auth/github", get(github_auth))
        .route("/auth/github/callback", get(github_callback))
        .route("/auth/token/rotate", post(rotate_token))
        // Lock routes
        .route("/locks/:name/acquire", post(acquire_lock))
        .route("/locks/:name/release", post(release_lock))
        .route("/locks/:name/renew", post(renew_lock))
        .route("/locks/:name", get(get_lock_status))
        .route("/locks", get(list_user_locks))
        // Rate limit routes
        .route("/limits/:name/check", post(check_rate_limit))
        .route("/limits/:name", get(get_rate_limit_status).delete(reset_rate_limit))
        .route("/limits", get(list_rate_limits))
        // Feature flag routes
        .route("/flags/:name", put(set_flag).get(get_flag).delete(delete_flag))
        .route("/flags", get(list_flags))
        // Config store routes
        .route("/config/:key", put(set_config).get(get_config).delete(delete_config))
        .route("/config/:key/history", get(get_config_history))
        .route("/config", get(list_configs))
        // Documentation routes
        .route("/openapi.yaml", get(openapi_spec))
        .route("/docs", get(api_docs))
        // Health check
        .route("/health", get(health_check))
        // Admin routes  
        .route("/admin/status", get(admin_status))
        // Metrics endpoint
        .route("/metrics", get(metrics_endpoint))
        // Add metrics middleware layer
        .layer(middleware::from_fn_with_state(app_state.clone(), metrics_middleware))
        // Add CORS layer
        .layer(
            CorsLayer::new()
                .allow_origin([
                    "https://octostore.io".parse().unwrap(),
                    "http://localhost:3000".parse().unwrap(),
                    "http://127.0.0.1:3000".parse().unwrap(),
                ])
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST, axum::http::Method::PUT, axum::http::Method::DELETE])
                .allow_headers([axum::http::header::AUTHORIZATION, axum::http::header::CONTENT_TYPE])
        )
        // Add state
        .with_state(app_state);

    // Create listener
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    info!("Server listening on {}", config.bind_addr);

    // Set up graceful shutdown handling
    let shutdown_lock_store = lock_store.clone();
    let shutdown_auth_service = Arc::new(auth_service);
    
    // Start the server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_lock_store, shutdown_auth_service))
        .await?;

    info!("Server stopped");
    Ok(())
}

async fn health_check() -> &'static str {
    "OK"
}

async fn admin_status(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<axum::Json<serde_json::Value>, axum::response::Response> {
    // Check admin key from header
    let provided_key = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let admin_key_valid = match (&provided_key, &state.config.admin_key) {
        (Some(provided), Some(expected)) => provided == expected,
        _ => false,
    };

    if !admin_key_valid {
        // Fall back to OAuth-based admin check
        let user_id = match state.auth_service.authenticate(&headers) {
            Ok(id) => id,
            Err(_) => {
                return Err(axum::response::Response::builder()
                    .status(401)
                    .body("Unauthorized: Provide X-Admin-Key header or valid Bearer token".into())
                    .unwrap());
            }
        };

        match state.auth_service.get_user_by_id(&user_id.to_string()) {
            Ok(Some(username)) if username == "aronchick" => {}
            _ => {
                return Err(axum::response::Response::builder()
                    .status(403)
                    .body("Forbidden: Admin access required".into())
                    .unwrap());
            }
        }
    }

    // Get all active locks
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    let locks: Vec<serde_json::Value> = active_locks
        .into_iter()
        .filter_map(|lock| {
            // Get holder username
            let holder_username = state.auth_service
                .get_user_by_id(&lock.holder_id.to_string())
                .unwrap_or(None)
                .unwrap_or_else(|| "unknown".to_string());

            let now = chrono::Utc::now();
            let ttl_remaining = if lock.expires_at > now {
                (lock.expires_at - now).num_seconds()
            } else {
                0
            };

            Some(serde_json::json!({
                "name": lock.name,
                "holder_username": holder_username,
                "metadata": lock.metadata,
                "fencing_token": lock.fencing_token,
                "expires_at": lock.expires_at.to_rfc3339(),
                "ttl_remaining_seconds": ttl_remaining
            }))
        }).collect();

    // Get all registered users
    let users = state.auth_service.get_all_users().unwrap_or_default();

    let uptime_seconds = state.metrics.start_time.elapsed().as_secs();
    let total_acquires = state.metrics.lock_store_acquires.load(std::sync::atomic::Ordering::Relaxed);
    let total_releases = state.metrics.lock_store_releases.load(std::sync::atomic::Ordering::Relaxed);
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