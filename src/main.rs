mod auth;
mod config;
mod error;
mod locks;
mod models;
mod store;

use auth::{github_auth, github_callback, rotate_token, AuthService};
use axum::{
    http::HeaderValue,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use config::Config;
use locks::{acquire_lock, get_lock_status, list_user_locks, release_lock, renew_lock, LockHandlers};

#[derive(Clone)]
pub struct AppState {
    pub lock_handlers: LockHandlers,
    pub auth_service: AuthService,
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
    let lock_store = LockStore::new(initial_fencing_token);
    
    // Start background expiry task
    let expiry_store = lock_store.clone();
    expiry_store.start_expiry_task();

    // Create app state
    let lock_handlers = LockHandlers::new(lock_store.clone(), auth_service.clone());
    let app_state = AppState {
        lock_handlers: lock_handlers.clone(),
        auth_service: auth_service.clone(),
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
        // Documentation routes
        .route("/openapi.yaml", get(openapi_spec))
        .route("/docs", get(api_docs))
        // Health check
        .route("/health", get(health_check))
        // Add CORS layer
        .layer(CorsLayer::permissive())
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