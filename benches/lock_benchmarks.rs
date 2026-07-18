use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use octostore::auth::AuthService;
use octostore::config::Config;
use octostore::store::{AcquireLockOptions, DbConn, LockStore};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tempfile::NamedTempFile;
use uuid::Uuid;

fn create_test_config() -> Config {
    let temp_file = NamedTempFile::new().unwrap();
    let db_path = temp_file.path().to_str().unwrap().to_string();
    std::mem::forget(temp_file); // Keep the SQLite file for the benchmark process.

    Config {
        bind_addr: "127.0.0.1:0".to_string(),
        database_url: db_path,
        github_client_id: Some("test_client_id".to_string()),
        github_client_secret: Some("test_client_secret".to_string()),
        github_redirect_uri: "http://localhost:3000/callback".to_string(),
        admin_key: Some("test_admin_key".to_string()),
        admin_username: None,
        static_tokens: None,
        static_tokens_file: None,
        public_elections_enabled: true,
        max_public_elections: 100,
    }
}

fn create_test_store() -> (LockStore, AuthService) {
    let config = create_test_config();
    let db: DbConn = Arc::new(Mutex::new(Connection::open(&config.database_url).unwrap()));
    let auth_service = AuthService::new(config, db.clone()).unwrap();
    let lock_store = LockStore::new(db, 0).unwrap();
    (lock_store, auth_service)
}

fn lock_options(ttl_seconds: u32) -> AcquireLockOptions {
    AcquireLockOptions::new(ttl_seconds).with_metadata(Some("test metadata".to_string()))
}

fn bench_acquire_lock(c: &mut Criterion) {
    let (store, _auth) = create_test_store();
    let user_id = Uuid::new_v4();

    c.bench_function("acquire_lock", |b| {
        b.iter(|| {
            let lock_name = format!("test-lock-{}", Uuid::new_v4());
            black_box(
                store
                    .acquire_lock(lock_name, user_id, lock_options(60))
                    .unwrap(),
            )
        })
    });
}

fn bench_release_lock(c: &mut Criterion) {
    let (store, _auth) = create_test_store();
    let user_id = Uuid::new_v4();

    c.bench_function("release_lock", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();

            for _ in 0..iters {
                let lock_name = format!("test-lock-{}", Uuid::new_v4());
                let (lease_id, _, _) = store
                    .acquire_lock(lock_name.clone(), user_id, lock_options(60))
                    .unwrap();
                store.release_lock(&lock_name, lease_id, user_id).unwrap();
                black_box(());
            }

            start.elapsed()
        })
    });
}

fn bench_acquire_release_cycle(c: &mut Criterion) {
    let (store, _auth) = create_test_store();
    let user_id = Uuid::new_v4();

    c.bench_function("acquire_release_cycle", |b| {
        b.iter(|| {
            let lock_name = format!("test-lock-{}", Uuid::new_v4());
            let (lease_id, _, _) = store
                .acquire_lock(lock_name.clone(), user_id, lock_options(60))
                .unwrap();
            store.release_lock(&lock_name, lease_id, user_id).unwrap();
            black_box(());
        })
    });
}

fn bench_contention_2_threads(c: &mut Criterion) {
    let (store, _auth) = create_test_store();
    let store = Arc::new(store);

    c.bench_function("contention_2_threads", |b| {
        b.iter(|| {
            let lock_name = format!("contested-lock-{}", Uuid::new_v4());
            let user1 = Uuid::new_v4();
            let user2 = Uuid::new_v4();
            let result1 = store.acquire_lock(lock_name.clone(), user1, lock_options(1));
            let result2 = store.acquire_lock(lock_name.clone(), user2, lock_options(1));

            if let Ok((lease_id, _, _)) = result1 {
                let _ = store.release_lock(&lock_name, lease_id, user1);
            }
            if let Ok((lease_id, _, _)) = result2 {
                let _ = store.release_lock(&lock_name, lease_id, user2);
            }

            black_box((result1.is_ok(), result2.is_ok()))
        })
    });
}

fn bench_contention_10_attempts(c: &mut Criterion) {
    let (store, _auth) = create_test_store();

    c.bench_function("contention_10_attempts", |b| {
        b.iter(|| {
            let lock_name = format!("contested-lock-10-{}", Uuid::new_v4());
            let mut acquired = Vec::new();

            for _ in 0..10 {
                let user_id = Uuid::new_v4();
                if let Ok((lease_id, _, _)) =
                    store.acquire_lock(lock_name.clone(), user_id, lock_options(1))
                {
                    acquired.push((user_id, lease_id));
                }
            }

            for (user_id, lease_id) in &acquired {
                let _ = store.release_lock(&lock_name, *lease_id, *user_id);
            }

            black_box(acquired.len())
        })
    });
}

fn bench_many_different_locks(c: &mut Criterion) {
    let (store, _auth) = create_test_store();

    c.bench_with_input(
        BenchmarkId::new("many_different_locks", 1000),
        &1000,
        |b, &count| {
            b.iter(|| {
                let user_id = Uuid::new_v4();
                let mut results = Vec::new();
                for i in 0..count {
                    let lock_name = format!("unique-lock-{}-{}", i, Uuid::new_v4());
                    let (lease_id, _, _) = store
                        .acquire_lock(lock_name.clone(), user_id, lock_options(60))
                        .unwrap();
                    results.push((lock_name, lease_id));
                }

                for (lock_name, lease_id) in results {
                    let _ = store.release_lock(&lock_name, lease_id, user_id);
                }

                black_box(count)
            })
        },
    );
}

fn bench_fencing_token_generation(c: &mut Criterion) {
    let (store, _auth) = create_test_store();
    let mut group = c.benchmark_group("fencing_token_generation");
    group.throughput(Throughput::Elements(1));
    group.bench_function("acquire_generates_token", |b| {
        b.iter(|| {
            let user_id = Uuid::new_v4();
            let lock_name = format!("test-lock-{}", Uuid::new_v4());
            let (lease_id, fencing_token, _) = store
                .acquire_lock(lock_name.clone(), user_id, lock_options(60))
                .unwrap();
            let _ = store.release_lock(&lock_name, lease_id, user_id);

            black_box(fencing_token)
        })
    });
    group.finish();
}

fn bench_sqlite_persistence(c: &mut Criterion) {
    let (store, _auth) = create_test_store();

    c.bench_function("sqlite_persistence", |b| {
        b.iter(|| {
            let user_id = Uuid::new_v4();
            let lock_name = format!("persist-lock-{}", Uuid::new_v4());
            let options =
                AcquireLockOptions::new(60).with_metadata(Some("large metadata ".repeat(100)));
            let result = store
                .acquire_lock(lock_name.clone(), user_id, options)
                .unwrap();
            let _ = store.release_lock(&lock_name, result.0, user_id);

            black_box(result)
        })
    });
}

criterion_group!(
    lock_benchmarks,
    bench_acquire_lock,
    bench_release_lock,
    bench_acquire_release_cycle,
    bench_contention_2_threads,
    bench_contention_10_attempts,
    bench_many_different_locks,
    bench_fencing_token_generation,
    bench_sqlite_persistence
);
criterion_main!(lock_benchmarks);
