use clap::Parser;
use reqwest::Client;
use serde_json::json;
use sha2::{Sha256, Digest};
use std::{
    sync::{
        atomic::{AtomicU64, AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    time::{interval, sleep},
    select,
    signal,
};
// uuid::Uuid removed - not needed

#[derive(Parser, Debug)]
#[command(name = "octostore-bench")]
#[command(about = "OctoStore Benchmark Tool")]
struct Args {
    /// Base URL
    #[arg(long, default_value = "https://api.octostore.io")]
    url: String,

    /// Bearer token (REQUIRED)
    #[arg(long)]
    token: String,

    /// Number of concurrent workers
    #[arg(long, default_value_t = 10)]
    concurrency: u64,

    /// Duration in seconds
    #[arg(long, default_value_t = 30)]
    duration: u64,

    /// Admin key (required to prevent abuse)
    #[arg(long)]
    admin_key: String,
}

struct Stats {
    acquire_latencies: Mutex<Vec<u128>>,
    status_latencies: Mutex<Vec<u128>>,
    renew_latencies: Mutex<Vec<u128>>,
    release_latencies: Mutex<Vec<u128>>,
    acquire_count: AtomicU64,
    status_count: AtomicU64,
    renew_count: AtomicU64,
    release_count: AtomicU64,
    error_count: AtomicU64,
    total_ops: AtomicU64,
    start_time: Instant,
    running: AtomicBool,
}

impl Stats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            acquire_latencies: Mutex::new(Vec::new()),
            status_latencies: Mutex::new(Vec::new()),
            renew_latencies: Mutex::new(Vec::new()),
            release_latencies: Mutex::new(Vec::new()),
            acquire_count: AtomicU64::new(0),
            status_count: AtomicU64::new(0),
            renew_count: AtomicU64::new(0),
            release_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            total_ops: AtomicU64::new(0),
            start_time: Instant::now(),
            running: AtomicBool::new(true),
        })
    }

    fn record_latency(&self, operation: &str, latency_ms: u128) {
        match operation {
            "acquire" => {
                self.acquire_latencies.lock().unwrap().push(latency_ms);
                self.acquire_count.fetch_add(1, Ordering::Relaxed);
            }
            "status" => {
                self.status_latencies.lock().unwrap().push(latency_ms);
                self.status_count.fetch_add(1, Ordering::Relaxed);
            }
            "renew" => {
                self.renew_latencies.lock().unwrap().push(latency_ms);
                self.renew_count.fetch_add(1, Ordering::Relaxed);
            }
            "release" => {
                self.release_latencies.lock().unwrap().push(latency_ms);
                self.release_count.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.total_ops.fetch_add(1, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

fn calculate_percentiles(mut values: Vec<u128>) -> (f64, f64, f64, f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0, 0.0, 0.0, 0.0);
    }

    values.sort_unstable();
    let len = values.len();

    let avg = values.iter().sum::<u128>() as f64 / len as f64;
    let p50 = values[len * 50 / 100] as f64;
    let p95 = values[len * 95 / 100] as f64;
    let p99 = values[len * 99 / 100] as f64;
    let max = values[len - 1] as f64;

    (avg, p50, p95, p99, max)
}

async fn benchmark_worker(
    worker_id: u64,
    client: Client,
    base_url: String,
    token: String,
    stats: Arc<Stats>,
    cleanup_locks: Arc<Mutex<Vec<String>>>,
) {
    let mut iteration = 0u64;

    while stats.is_running() {
        iteration += 1;
        let lock_name = format!("bench-{}-{}", worker_id, iteration);
        
        // Store lock name for cleanup
        cleanup_locks.lock().unwrap().push(lock_name.clone());

        // Acquire lock
        let start = Instant::now();
        let acquire_result = client
            .post(&format!("{}/locks/{}/acquire", base_url, lock_name))
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({"ttl_seconds": 60}))
            .send()
            .await;

        let acquire_latency = start.elapsed().as_millis();

        let lease_id = match acquire_result {
            Ok(response) if response.status().is_success() => {
                match response.json::<serde_json::Value>().await {
                    Ok(json) => {
                        stats.record_latency("acquire", acquire_latency);
                        json.get("lease_id").and_then(|s| s.as_str()).map(|s| s.to_string())
                    }
                    Err(_) => {
                        stats.record_error();
                        None
                    }
                }
            }
            _ => {
                stats.record_error();
                None
            }
        };

        if let Some(lease_id) = lease_id {
            if !stats.is_running() { break; }

            // Check lock status
            let start = Instant::now();
            let status_result = client
                .get(&format!("{}/locks/{}", base_url, lock_name))
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await;

            let status_latency = start.elapsed().as_millis();

            match status_result {
                Ok(response) if response.status().is_success() => {
                    stats.record_latency("status", status_latency);
                }
                _ => {
                    stats.record_error();
                }
            }

            if !stats.is_running() { break; }

            // Renew lock
            let start = Instant::now();
            let renew_result = client
                .post(&format!("{}/locks/{}/renew", base_url, lock_name))
                .header("Authorization", format!("Bearer {}", token))
                .json(&json!({"lease_id": lease_id, "ttl_seconds": 60}))
                .send()
                .await;

            let renew_latency = start.elapsed().as_millis();

            match renew_result {
                Ok(response) if response.status().is_success() => {
                    stats.record_latency("renew", renew_latency);
                }
                _ => {
                    stats.record_error();
                }
            }

            if !stats.is_running() { break; }

            // Release lock
            let start = Instant::now();
            let release_result = client
                .post(&format!("{}/locks/{}/release", base_url, lock_name))
                .header("Authorization", format!("Bearer {}", token))
                .json(&json!({"lease_id": lease_id}))
                .send()
                .await;

            let release_latency = start.elapsed().as_millis();

            match release_result {
                Ok(response) if response.status().is_success() => {
                    stats.record_latency("release", release_latency);
                }
                _ => {
                    stats.record_error();
                }
            }
        }

        // Small delay to prevent overwhelming the server
        sleep(Duration::from_millis(1)).await;
    }
}

fn draw_progress_bar(elapsed: u64, total: u64) -> String {
    let percentage = (elapsed as f64 / total as f64).min(1.0);
    let filled = (40.0 * percentage) as usize;
    let empty = 40 - filled;
    format!("{}{}", "━".repeat(filled), "━".repeat(empty))
}

async fn print_live_stats(stats: Arc<Stats>, total_duration: u64, concurrency: u64) {
    let mut interval = interval(Duration::from_secs(1));
    
    while stats.is_running() {
        select! {
            _ = interval.tick() => {
                let elapsed = stats.start_time.elapsed().as_secs();
                if elapsed > total_duration {
                    break;
                }

                let total_ops = stats.total_ops.load(Ordering::Relaxed);
                let acquires = stats.acquire_count.load(Ordering::Relaxed);
                let statuses = stats.status_count.load(Ordering::Relaxed);
                let renews = stats.renew_count.load(Ordering::Relaxed);
                let releases = stats.release_count.load(Ordering::Relaxed);
                let errors = stats.error_count.load(Ordering::Relaxed);

                let ops_per_sec = if elapsed > 0 {
                    total_ops as f64 / elapsed as f64
                } else {
                    0.0
                };

                // Calculate current stats for live display
                let (acquire_avg, acquire_p50, acquire_p95, acquire_p99, _) = {
                    let latencies = stats.acquire_latencies.lock().unwrap();
                    calculate_percentiles(latencies.clone())
                };

                let (status_avg, status_p50, status_p95, status_p99, _) = {
                    let latencies = stats.status_latencies.lock().unwrap();
                    calculate_percentiles(latencies.clone())
                };

                let (renew_avg, renew_p50, renew_p95, renew_p99, _) = {
                    let latencies = stats.renew_latencies.lock().unwrap();
                    calculate_percentiles(latencies.clone())
                };

                let (release_avg, release_p50, release_p95, release_p99, _) = {
                    let latencies = stats.release_latencies.lock().unwrap();
                    calculate_percentiles(latencies.clone())
                };

                // Clear line and move cursor up to overwrite previous output
                print!("\r\x1b[K\x1b[8A\x1b[K");

                println!("\x1b[1m🐙 OctoStore Benchmark — {} workers, {}s\x1b[0m", concurrency, total_duration);
                println!("{}", draw_progress_bar(elapsed, total_duration));
                println!(" \x1b[32m⏱\x1b[0m  Elapsed: {}s / {}s", elapsed, total_duration);
                println!(" \x1b[36m📊\x1b[0m Operations: {} total ({:.1} ops/sec)", total_ops, ops_per_sec);
                println!(" \x1b[33m🔒\x1b[0m Acquires:   {} (avg {:.1}ms, p50 {:.1}ms, p95 {:.1}ms, p99 {:.1}ms)", 
                    acquires, acquire_avg, acquire_p50, acquire_p95, acquire_p99);
                println!(" \x1b[34m👀\x1b[0m Statuses:   {} (avg {:.1}ms, p50 {:.1}ms, p95 {:.1}ms, p99 {:.1}ms)", 
                    statuses, status_avg, status_p50, status_p95, status_p99);
                println!(" \x1b[35m🔄\x1b[0m Renews:     {} (avg {:.1}ms, p50 {:.1}ms, p95 {:.1}ms, p99 {:.1}ms)", 
                    renews, renew_avg, renew_p50, renew_p95, renew_p99);
                println!(" \x1b[36m🔓\x1b[0m Releases:   {} (avg {:.1}ms, p50 {:.1}ms, p95 {:.1}ms, p99 {:.1}ms)", 
                    releases, release_avg, release_p50, release_p95, release_p99);
                let error_color = if errors > 0 { "\x1b[31m" } else { "\x1b[32m" };
                println!(" {}❌\x1b[0m Errors:     {}", error_color, errors);
            }
            _ = signal::ctrl_c() => {
                stats.stop();
                break;
            }
        }
    }
}

async fn cleanup_locks(client: &Client, base_url: &str, token: &str, locks: &[String]) {
    if locks.is_empty() {
        return;
    }

    println!("\n🧹 Cleaning up {} benchmark locks...", locks.len());
    
    for lock_name in locks {
        // We don't have the lease_id, but we can try to release with a dummy one
        // This might fail but that's okay - locks will expire anyway
        let _ = client
            .post(&format!("{}/locks/{}/release", base_url, lock_name))
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({"lease_id": "00000000-0000-0000-0000-000000000000"}))
            .send()
            .await;
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Validate admin key
    let expected_hash = "13478763f37c2e09df7a315a960ec607b69179f58f60b5aa91e3f1c292c77698";
    let mut hasher = Sha256::new();
    hasher.update(args.admin_key.as_bytes());
    let provided_hash = format!("{:x}", hasher.finalize());

    if provided_hash != expected_hash {
        println!("❌ Invalid admin key");
        std::process::exit(1);
    }

    let base_url = args.url.trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to create HTTP client");

    let stats = Stats::new();
    let lock_list_for_cleanup = Arc::new(Mutex::new(Vec::new()));

    println!("\n\x1b[1m🐙 OctoStore Benchmark — {} workers, {}s\x1b[0m", args.concurrency, args.duration);
    println!("{}", "━".repeat(40));
    println!(" \x1b[32m⏱\x1b[0m  Elapsed: 0s / {}s", args.duration);
    println!(" \x1b[36m📊\x1b[0m Operations: 0 total (0.0 ops/sec)");
    println!(" \x1b[33m🔒\x1b[0m Acquires:   0 (avg 0.0ms, p50 0.0ms, p95 0.0ms, p99 0.0ms)");
    println!(" \x1b[34m👀\x1b[0m Statuses:   0 (avg 0.0ms, p50 0.0ms, p95 0.0ms, p99 0.0ms)");
    println!(" \x1b[35m🔄\x1b[0m Renews:     0 (avg 0.0ms, p50 0.0ms, p95 0.0ms, p99 0.0ms)");
    println!(" \x1b[36m🔓\x1b[0m Releases:   0 (avg 0.0ms, p50 0.0ms, p95 0.0ms, p99 0.0ms)");
    println!(" \x1b[32m❌\x1b[0m Errors:     0");

    // Start live stats display task
    let stats_clone = Arc::clone(&stats);
    let live_stats_task = tokio::spawn(print_live_stats(stats_clone, args.duration, args.concurrency));

    // Start worker tasks
    let mut handles = Vec::new();
    for worker_id in 0..args.concurrency {
        let client_clone = client.clone();
        let base_url_clone = base_url.to_string();
        let token_clone = args.token.clone();
        let stats_clone = Arc::clone(&stats);
        let cleanup_locks_clone = Arc::clone(&cleanup_locks);

        let handle = tokio::spawn(benchmark_worker(
            worker_id,
            client_clone,
            base_url_clone,
            token_clone,
            stats_clone,
            cleanup_locks_clone,
        ));
        handles.push(handle);
    }

    // Run for the specified duration
    sleep(Duration::from_secs(args.duration)).await;
    stats.stop();

    // Wait for all workers to finish
    for handle in handles {
        let _ = handle.await;
    }

    // Cancel live stats task
    live_stats_task.abort();

    // Get final stats
    let final_duration = stats.start_time.elapsed().as_secs_f64();
    let total_ops = stats.total_ops.load(Ordering::Relaxed);
    let acquires = stats.acquire_count.load(Ordering::Relaxed);
    let statuses = stats.status_count.load(Ordering::Relaxed);
    let renews = stats.renew_count.load(Ordering::Relaxed);
    let releases = stats.release_count.load(Ordering::Relaxed);
    let errors = stats.error_count.load(Ordering::Relaxed);

    let (acquire_avg, acquire_p50, acquire_p95, acquire_p99, acquire_max) = {
        let latencies = stats.acquire_latencies.lock().unwrap();
        calculate_percentiles(latencies.clone())
    };

    let (status_avg, status_p50, status_p95, status_p99, status_max) = {
        let latencies = stats.status_latencies.lock().unwrap();
        calculate_percentiles(latencies.clone())
    };

    let (renew_avg, renew_p50, renew_p95, renew_p99, renew_max) = {
        let latencies = stats.renew_latencies.lock().unwrap();
        calculate_percentiles(latencies.clone())
    };

    let (release_avg, release_p50, release_p95, release_p99, release_max) = {
        let latencies = stats.release_latencies.lock().unwrap();
        calculate_percentiles(latencies.clone())
    };

    // Clean up benchmark locks
    let cleanup_list = cleanup_locks.lock().unwrap().clone();
    cleanup_locks(&client, base_url, &args.token, &cleanup_list).await;

    // Print final summary
    println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("\x1b[1m🏁 Benchmark Complete\x1b[0m\n");

    println!(" Duration:     {:.1}s", final_duration);
    println!(" Workers:      {}", args.concurrency);
    println!(" Total Ops:    {} ({:.1} ops/sec)\n", total_ops, total_ops as f64 / final_duration);

    println!(" Operation     Count    Avg     p50     p95     p99     Max");
    println!(" ─────────────────────────────────────────────────────────");
    println!(" Acquire       {:>5}    {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>6.1}ms", 
        acquires, acquire_avg, acquire_p50, acquire_p95, acquire_p99, acquire_max);
    println!(" Status        {:>5}    {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>6.1}ms", 
        statuses, status_avg, status_p50, status_p95, status_p99, status_max);
    println!(" Renew         {:>5}    {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>6.1}ms", 
        renews, renew_avg, renew_p50, renew_p95, renew_p99, renew_max);
    println!(" Release       {:>5}    {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>6.1}ms", 
        releases, release_avg, release_p50, release_p95, release_p99, release_max);

    println!();
    let error_percentage = if total_ops > 0 { errors as f64 / total_ops as f64 * 100.0 } else { 0.0 };
    let error_color = if errors > 0 { "\x1b[31m" } else { "\x1b[32m" };
    println!(" {}Errors:       {} ({:.2}%)\x1b[0m", error_color, errors, error_percentage);
    println!(" \x1b[32mThroughput:   {:.1} ops/sec\x1b[0m", total_ops as f64 / final_duration);

    // Always exit 0 (it's a benchmark, not a test)
    std::process::exit(0);
}