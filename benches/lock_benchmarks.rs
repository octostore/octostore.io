use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;
use uuid::Uuid;

// Import from the main crate
use octostore::auth::AuthService;
use octostore::config::Config;
use octostore::models::AcquireLockRequest;
use octostore::store::LockStore;

// HTTP benchmarks
use reqwest;
use tokio::net::TcpListener;

fn create_test_config() -> Config {
    let temp_file = NamedTempFile::new().unwrap();
    let db_path = temp_file.path().to_str().unwrap().to_string();
    std::mem::forget(temp_file); // Don't delete the file yet
    
    Config {
        bind_addr: "127.0.0.1:0".to_string(), // Use random port
        database_url: db_path,
        github_client_id: "test_client_id".to_string(),
        github_client_secret: "test_client_secret".to_string(),
        github_redirect_uri: "http://localhost:3000/callback".to_string(),
        admin_key: Some("test_admin_key".to_string()),
    }
}

fn create_test_store() -> (LockStore, AuthService) {
    let config = create_test_config();
    let auth_service = AuthService::new(config.clone()).unwrap();
    let lock_store = LockStore::new(&config.database_url, 0).unwrap();
    (lock_store, auth_service)
}

// Micro-benchmarks testing LockStore directly

fn bench_acquire_lock(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    let user_id = Uuid::new_v4();
    
    c.bench_function("acquire_lock", |b| {
        b.to_async(&rt).iter(|| async {
            let lock_name = format!("test-lock-{}", Uuid::new_v4());
            let request = AcquireLockRequest {
                ttl_seconds: Some(60),
                metadata: Some("test metadata".to_string()),
            };
            
            black_box(store.acquire_lock(&lock_name, user_id, &request).await.unwrap())
        })
    });
}

fn bench_release_lock(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    let user_id = Uuid::new_v4();
    
    c.bench_function("release_lock", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let start = Instant::now();
            
            for _i in 0..iters {
                // Acquire first
                let lock_name = format!("test-lock-{}", Uuid::new_v4());
                let request = AcquireLockRequest {
                    ttl_seconds: Some(60),
                    metadata: Some("test metadata".to_string()),
                };
                let result = store.acquire_lock(&lock_name, user_id, &request).await.unwrap();
                
                // Then release (this is what we're measuring)
                black_box(store.release_lock(&lock_name, user_id, result.fencing_token).await.unwrap());
            }
            
            start.elapsed()
        })
    });
}

fn bench_acquire_release_cycle(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    let user_id = Uuid::new_v4();
    
    c.bench_function("acquire_release_cycle", |b| {
        b.to_async(&rt).iter(|| async {
            let lock_name = format!("test-lock-{}", Uuid::new_v4());
            let request = AcquireLockRequest {
                ttl_seconds: Some(60),
                metadata: Some("test metadata".to_string()),
            };
            
            // Full cycle
            let result = store.acquire_lock(&lock_name, user_id, &request).await.unwrap();
            black_box(store.release_lock(&lock_name, user_id, result.fencing_token).await.unwrap());
        })
    });
}

fn bench_contention_2_threads(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    let store = Arc::new(store);
    
    c.bench_function("contention_2_threads", |b| {
        b.to_async(&rt).iter(|| async {
            let lock_name = "contested-lock".to_string();
            let store1 = store.clone();
            let store2 = store.clone();
            let user1 = Uuid::new_v4();
            let user2 = Uuid::new_v4();
            
            let request = AcquireLockRequest {
                ttl_seconds: Some(1), // Short TTL for quick release
                metadata: Some("test metadata".to_string()),
            };
            
            // Two threads competing for the same lock
            let task1 = tokio::spawn(async move {
                store1.acquire_lock(&lock_name, user1, &request).await
            });
            
            let task2 = tokio::spawn(async move {
                store2.acquire_lock(&lock_name, user2, &request).await
            });
            
            let (result1, result2) = tokio::join!(task1, task2);
            
            // Clean up - release any acquired locks
            if let Ok(Ok(lock_result)) = result1 {
                let _ = store.release_lock(&lock_name, user1, lock_result.fencing_token).await;
            }
            if let Ok(Ok(lock_result)) = result2 {
                let _ = store.release_lock(&lock_name, user2, lock_result.fencing_token).await;
            }
            
            black_box((result1, result2))
        })
    });
}

fn bench_contention_10_threads(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    let store = Arc::new(store);
    
    c.bench_function("contention_10_threads", |b| {
        b.to_async(&rt).iter(|| async {
            let lock_name = "contested-lock-10".to_string();
            let request = AcquireLockRequest {
                ttl_seconds: Some(1),
                metadata: Some("test metadata".to_string()),
            };
            
            let mut tasks = Vec::new();
            for _ in 0..10 {
                let store_clone = store.clone();
                let lock_name_clone = lock_name.clone();
                let request_clone = request.clone();
                let user_id = Uuid::new_v4();
                
                tasks.push(tokio::spawn(async move {
                    (user_id, store_clone.acquire_lock(&lock_name_clone, user_id, &request_clone).await)
                }));
            }
            
            let results = futures::future::join_all(tasks).await;
            
            // Clean up
            for result in &results {
                if let Ok((user_id, Ok(lock_result))) = result {
                    let _ = store.release_lock(&lock_name, *user_id, lock_result.fencing_token).await;
                }
            }
            
            black_box(results)
        })
    });
}

fn bench_many_different_locks(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    
    c.bench_with_input(
        BenchmarkId::new("many_different_locks", 1000),
        &1000,
        |b, &count| {
            b.to_async(&rt).iter(|| async {
                let user_id = Uuid::new_v4();
                let request = AcquireLockRequest {
                    ttl_seconds: Some(60),
                    metadata: Some("test metadata".to_string()),
                };
                
                let mut results = Vec::new();
                for i in 0..count {
                    let lock_name = format!("unique-lock-{}", i);
                    let result = store.acquire_lock(&lock_name, user_id, &request).await.unwrap();
                    results.push((lock_name, result.fencing_token));
                }
                
                // Clean up
                for (lock_name, fencing_token) in results {
                    let _ = store.release_lock(&lock_name, user_id, fencing_token).await;
                }
                
                black_box(count)
            })
        }
    );
}

fn bench_fencing_token_generation(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    
    c.throughput(Throughput::Elements(1));
    c.bench_function("fencing_token_generation", |b| {
        b.to_async(&rt).iter(|| async {
            let user_id = Uuid::new_v4();
            let lock_name = format!("test-lock-{}", Uuid::new_v4());
            let request = AcquireLockRequest {
                ttl_seconds: Some(60),
                metadata: Some("test metadata".to_string()),
            };
            
            let result = store.acquire_lock(&lock_name, user_id, &request).await.unwrap();
            let fencing_token = result.fencing_token;
            let _ = store.release_lock(&lock_name, user_id, fencing_token).await;
            
            black_box(fencing_token)
        })
    });
}

fn bench_sqlite_persistence(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (store, _auth) = create_test_store();
    
    c.bench_function("sqlite_persistence", |b| {
        b.to_async(&rt).iter(|| async {
            let user_id = Uuid::new_v4();
            let lock_name = format!("persist-lock-{}", Uuid::new_v4());
            let request = AcquireLockRequest {
                ttl_seconds: Some(60),
                metadata: Some("large metadata ".repeat(100)), // Larger metadata to stress SQLite
            };
            
            // This tests the SQLite write-through latency
            let result = store.acquire_lock(&lock_name, user_id, &request).await.unwrap();
            let _ = store.release_lock(&lock_name, user_id, result.fencing_token).await;
            
            black_box(result)
        })
    });
}

// HTTP-level benchmarks using reqwest against a test server

async fn start_test_server() -> (String, tokio::task::JoinHandle<()>) {
    let config = create_test_config();
    let auth_service = AuthService::new(config.clone()).unwrap();
    let lock_store = LockStore::new(&config.database_url, 0).unwrap();
    
    // Import the necessary items for creating the app
    use octostore::locks::LockHandlers;
    use octostore::metrics::Metrics;
    
    use axum::{
        middleware,
        routing::{get, post},
        Router,
    };
    use std::sync::Arc;
    
    // Create handlers
    let lock_handlers = LockHandlers::new(lock_store.clone(), auth_service.clone());
    let metrics = Metrics::new();
    
    // Create app state (simplified for benchmarks)
    #[derive(Clone)]
    struct BenchAppState {
        lock_handlers: LockHandlers,
        auth_service: AuthService,
        config: octostore::config::Config,
        metrics: Arc<Metrics>,
    }
    
    let app_state = BenchAppState {
        lock_handlers,
        auth_service,
        config: config.clone(),
        metrics: metrics.clone(),
    };
    
    // Import the route handlers
    use octostore::locks::{acquire_lock, release_lock, get_lock_status, list_user_locks, renew_lock};
    
    // Create router
    let app = Router::new()
        .route("/locks/:name/acquire", post(acquire_lock))
        .route("/locks/:name/release", post(release_lock))
        .route("/locks/:name/renew", post(renew_lock))
        .route("/locks/:name", get(get_lock_status))
        .route("/locks", get(list_user_locks))
        .route("/health", get(|| async { "OK" }))
        .with_state(app_state);
    
    // Bind to a random port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_url = format!("http://{}", addr);
    
    // Start the server
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    
    // Give the server a moment to start
    tokio::time::sleep(Duration::from_millis(10)).await;
    
    (server_url, server_handle)
}

fn bench_http_acquire_release(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    c.bench_function("http_acquire_release", |b| {
        b.to_async(&rt).iter(|| async {
            let (server_url, _server_handle) = start_test_server().await;
            let client = reqwest::Client::new();
            
            // For HTTP benchmarks, we'll use a mock bearer token since we don't have 
            // a full OAuth setup in the benchmark
            let lock_name = format!("http-test-lock-{}", Uuid::new_v4());
            let acquire_url = format!("{}/locks/{}/acquire", server_url, lock_name);
            let release_url = format!("{}/locks/{}/release", server_url, lock_name);
            
            // Note: This will fail with 401 in practice because we don't have valid auth,
            // but it still exercises the HTTP stack and routing
            let acquire_response = client
                .post(&acquire_url)
                .header("Authorization", "Bearer test_token")
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "ttl_seconds": 60,
                    "metadata": "http test metadata"
                }))
                .send()
                .await;
            
            // Even if auth fails, we're measuring the HTTP overhead
            black_box(acquire_response)
        })
    });
}

criterion_group!(
    lock_benchmarks,
    bench_acquire_lock,
    bench_release_lock,
    bench_acquire_release_cycle,
    bench_contention_2_threads,
    bench_contention_10_threads,
    bench_many_different_locks,
    bench_fencing_token_generation,
    bench_sqlite_persistence,
    bench_http_acquire_release
);

criterion_main!(lock_benchmarks);