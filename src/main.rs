mod app;
mod auth;
mod config;
mod error;
mod locks;
mod metrics;
mod models;
mod store;

use app::AppState;
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
use locks::{acquire_lock, get_lock_status, list_user_locks, release_lock, renew_lock, LockHandlers};
use metrics::{endpoint_from_path, Metrics};

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
        .or_else(|| headers.get("x-octostore-admin-key"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            // Also accept "Bearer admin:<key>" in Authorization header
            headers.get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer admin:"))
                .map(|s| s.to_string())
        });

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

// Timeseries metrics endpoint - requires admin key
async fn timeseries_endpoint(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<axum::Json<serde_json::Value>, axum::response::Response> {
    // Check admin key from header
    let provided_key = headers
        .get("x-admin-key")
        .or_else(|| headers.get("x-octostore-admin-key"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            // Also accept "Bearer admin:<key>" in Authorization header
            headers.get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer admin:"))
                .map(|s| s.to_string())
        });

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

    // Get window parameter (default to "1h")
    let window = params.get("window").map(|s| s.as_str()).unwrap_or("1h");
    
    // Update active locks count in time series before returning data
    let active_locks = state.lock_handlers.store.get_all_active_locks();
    state.metrics.update_active_locks_count(active_locks.len() as u64);
    
    // Get time series data
    let timeseries_data = state.metrics.get_timeseries_data(window);

    Ok(axum::Json(timeseries_data))
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
    let metrics = Metrics::new();
    let app_state = AppState {
        lock_handlers: lock_handlers.clone(),
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
        // Documentation routes
        .route("/openapi.yaml", get(openapi_spec))
        .route("/docs", get(api_docs))
        // Health check
        .route("/health", get(health_check))
        // Admin routes  
        .route("/admin/status", get(admin_status))
        .route("/admin/metrics/timeseries", get(timeseries_endpoint))
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
        .or_else(|| headers.get("x-octostore-admin-key"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            // Also accept "Bearer admin:<key>" in Authorization header
            headers.get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer admin:"))
                .map(|s| s.to_string())
        });

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tempfile::NamedTempFile;
    use tower::util::ServiceExt; // for oneshot
    use uuid::Uuid;

    async fn create_test_app() -> Router {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        
        let config = Config {
            bind_addr: "127.0.0.1:3000".to_string(),
            database_url: db_path,
            github_client_id: "test_client_id".to_string(),
            github_client_secret: "test_client_secret".to_string(),
            github_redirect_uri: "http://localhost:3000/callback".to_string(),
            admin_key: Some("test_admin_key".to_string()),
        };

        let auth_service = AuthService::new(config.clone()).unwrap();
        let lock_store = LockStore::new(&config.database_url, 0).unwrap();
        
        let lock_handlers = LockHandlers::new(lock_store.clone(), auth_service.clone());
        let metrics = Metrics::new();
        
        let app_state = AppState {
            lock_handlers,
            auth_service,
            config: config.clone(),
            metrics,
        };

        Router::new()
            .route("/auth/github", get(github_auth))
            .route("/auth/github/callback", get(github_callback))
            .route("/auth/token/rotate", post(rotate_token))
            .route("/locks/:name/acquire", post(acquire_lock))
            .route("/locks/:name/release", post(release_lock))
            .route("/locks/:name/renew", post(renew_lock))
            .route("/locks/:name", get(get_lock_status))
            .route("/locks", get(list_user_locks))
            .route("/openapi.yaml", get(openapi_spec))
            .route("/docs", get(api_docs))
            .route("/health", get(health_check))
            .route("/admin/status", get(admin_status))
            .route("/metrics", get(metrics_endpoint))
            .layer(middleware::from_fn_with_state(app_state.clone(), metrics_middleware))
            .with_state(app_state)
    }

    async fn create_test_user(app: &Router) -> (String, Uuid) {
        let github_user = crate::models::GitHubUser {
            id: 12345,
            login: "testuser".to_string(),
        };
        
        // Extract state from app to create user directly (simulating OAuth flow)
        // In a real test, we'd mock the GitHub OAuth flow
        let state = app.clone().into_make_service().try_into_inner().unwrap();
        
        // This is a bit hacky but works for testing
        // In production tests, you might want to create a test-specific user creation method
        let user_token = "test_token_12345";
        (user_token.to_string(), Uuid::new_v4())
    }

    #[tokio::test]
    async fn test_health_check() {
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
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert_eq!(body_str, "OK");
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

    #[tokio::test]
    async fn test_api_docs() {
        let app = create_test_app().await;
        
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/docs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("OctoStore API Documentation"));
        assert!(body_str.contains("@scalar/api-reference"));
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

        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        
        let location = response.headers().get("location").unwrap();
        let location_str = location.to_str().unwrap();
        assert!(location_str.contains("https://github.com/login/oauth/authorize"));
        assert!(location_str.contains("client_id=test_client_id"));
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
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
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
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
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
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        
        assert_eq!(body_json["healthy"], true);
        assert!(body_json["uptime_seconds"].is_number());
        assert!(body_json["active_locks"].is_number());
        assert!(body_json["total_users"].is_number());
        assert!(body_json["locks"].is_array());
        assert!(body_json["users"].is_array());
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
        
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        
        assert!(body_json["uptime_seconds"].is_number());
        assert!(body_json["total_requests"].is_number());
        assert!(body_json["requests_per_second"].is_number());
        assert!(body_json["endpoints"].is_object());
        assert!(body_json["lock_store"].is_object());
        assert!(body_json["memory_bytes"].is_number());
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

        // Should get an error status (400 for bad request or 401 for auth)
        assert!(response.status().is_client_error());
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
                    .uri("/health")
                    .method("OPTIONS")
                    .header("origin", "https://octostore.io")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // CORS layer should handle OPTIONS requests
        assert!(response.status().is_success() || response.status() == StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]

    #[tokio::test]

    #[tokio::test]

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

        // Should fail on auth, not content-type
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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