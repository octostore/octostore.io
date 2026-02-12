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

#[derive(Parser, Debug)]
#[command(name = "octostore-bench")]
#[command(about = "OctoStore Stress Test & Benchmark")]
struct Args {
    /// Base URL
    #[arg(long, default_value = "https://api.octostore.io")]
    url: String,

    /// Bearer token (REQUIRED)
    #[arg(long)]
    token: String,

    /// Number of concurrent workers
    #[arg(long, default_value_t = 100)]
    concurrency: u64,

    /// Duration in seconds
    #[arg(long, default_value_t = 30)]
    duration: u64,

    /// Number of shared locks workers compete over (contention)
    #[arg(long, default_value_t = 10)]
    locks: u64,

    /// Admin key (required to prevent abuse)
    #[arg(long)]
    admin_key: String,
}

struct Stats {
    acquire_success: AtomicU64,
    acquire_contention: AtomicU64,  // Lock already held by someone else
    acquire_latencies: Mutex<Vec<u128>>,
    status_count: AtomicU64,
    status_latencies: Mutex<Vec<u128>>,
    renew_success: AtomicU64,
    renew_fail: AtomicU64,
    renew_latencies: Mutex<Vec<u128>>,
    release_success: AtomicU64,
    release_fail: AtomicU64,
    release_latencies: Mutex<Vec<u128>>,
    error_count: AtomicU64,  // network/unexpected errors
    total_ops: AtomicU64,
    start_time: Instant,
    running: AtomicBool,
}

impl Stats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            acquire_success: AtomicU64::new(0),
            acquire_contention: AtomicU64::new(0),
            acquire_latencies: Mutex::new(Vec::new()),
            status_count: AtomicU64::new(0),
            status_latencies: Mutex::new(Vec::new()),
            renew_success: AtomicU64::new(0),
            renew_fail: AtomicU64::new(0),
            renew_latencies: Mutex::new(Vec::new()),
            release_success: AtomicU64::new(0),
            release_fail: AtomicU64::new(0),
            release_latencies: Mutex::new(Vec::new()),
            error_count: AtomicU64::new(0),
            total_ops: AtomicU64::new(0),
            start_time: Instant::now(),
            running: AtomicBool::new(true),
        })
    }

    fn stop(&self) { self.running.store(false, Ordering::Relaxed); }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }
}

fn percentiles(mut v: Vec<u128>) -> (f64, f64, f64, f64, f64) {
    if v.is_empty() { return (0.0, 0.0, 0.0, 0.0, 0.0); }
    v.sort_unstable();
    let n = v.len();
    let avg = v.iter().sum::<u128>() as f64 / n as f64;
    (avg, v[n*50/100] as f64, v[n*95/100] as f64, v[n.saturating_mul(99)/100] as f64, v[n-1] as f64)
}

/// Worker that aggressively competes for a shared set of locks.
/// Each worker picks a random lock from the pool, tries to acquire it,
/// and if it wins, does status/renew/release. If it loses (contention),
/// that's tracked as a successful contention event, not an error.
async fn stress_worker(
    worker_id: u64,
    client: Client,
    base_url: String,
    token: String,
    num_locks: u64,
    stats: Arc<Stats>,
) {
    // Simple fast pseudo-random (xorshift)
    let mut rng = worker_id.wrapping_mul(2654435761) ^ 0xdeadbeef;
    
    while stats.is_running() {
        // Pick a random lock to compete for
        rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
        let lock_idx = rng % num_locks;
        let lock_name = format!("stress-lock-{}", lock_idx);

        // Try to acquire
        let start = Instant::now();
        let result = client
            .post(&format!("{}/locks/{}/acquire", base_url, lock_name))
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({"ttl_seconds": 10}))
            .send()
            .await;
        let latency = start.elapsed().as_millis();
        stats.total_ops.fetch_add(1, Ordering::Relaxed);

        match result {
            Ok(resp) => {
                let status = resp.status();
                
                // Auth failure = fatal, stop immediately
                if status.as_u16() == 401 {
                    eprintln!("\n\x1b[31m❌ Authentication failed! Your token is invalid.\x1b[0m");
                    eprintln!("   Sign in first: GET {}/auth/github\n", base_url);
                    stats.stop();
                    return;
                }
                
                match resp.json::<serde_json::Value>().await {
                    Ok(json) => {
                        stats.acquire_latencies.lock().unwrap().push(latency);
                        if status.is_success() {
                            let acquired = json.get("status")
                                .and_then(|s| s.as_str())
                                .map(|s| s == "acquired")
                                .unwrap_or(false);
                            
                            if acquired {
                                stats.acquire_success.fetch_add(1, Ordering::Relaxed);
                                let lease_id = json.get("lease_id")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();

                                if !stats.is_running() || lease_id.is_empty() { continue; }

                                // Won the lock! Do status check
                                let start = Instant::now();
                                if let Ok(resp) = client
                                    .get(&format!("{}/locks/{}", base_url, lock_name))
                                    .header("Authorization", format!("Bearer {}", token))
                                    .send().await
                                {
                                    let lat = start.elapsed().as_millis();
                                    if resp.status().is_success() {
                                        stats.status_count.fetch_add(1, Ordering::Relaxed);
                                        stats.status_latencies.lock().unwrap().push(lat);
                                    }
                                    stats.total_ops.fetch_add(1, Ordering::Relaxed);
                                }

                                if !stats.is_running() { continue; }

                                // Renew
                                let start = Instant::now();
                                if let Ok(resp) = client
                                    .post(&format!("{}/locks/{}/renew", base_url, lock_name))
                                    .header("Authorization", format!("Bearer {}", token))
                                    .json(&json!({"lease_id": lease_id, "ttl_seconds": 10}))
                                    .send().await
                                {
                                    let lat = start.elapsed().as_millis();
                                    stats.total_ops.fetch_add(1, Ordering::Relaxed);
                                    if resp.status().is_success() {
                                        stats.renew_success.fetch_add(1, Ordering::Relaxed);
                                        stats.renew_latencies.lock().unwrap().push(lat);
                                    } else {
                                        stats.renew_fail.fetch_add(1, Ordering::Relaxed);
                                    }
                                }

                                if !stats.is_running() { continue; }

                                // Release
                                let start = Instant::now();
                                if let Ok(resp) = client
                                    .post(&format!("{}/locks/{}/release", base_url, lock_name))
                                    .header("Authorization", format!("Bearer {}", token))
                                    .json(&json!({"lease_id": lease_id}))
                                    .send().await
                                {
                                    let lat = start.elapsed().as_millis();
                                    stats.total_ops.fetch_add(1, Ordering::Relaxed);
                                    if resp.status().is_success() {
                                        stats.release_success.fetch_add(1, Ordering::Relaxed);
                                        stats.release_latencies.lock().unwrap().push(lat);
                                    } else {
                                        stats.release_fail.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            } else {
                                // Lock held by someone else — contention! This is expected.
                                stats.acquire_contention.fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            // HTTP error but got JSON (e.g. 409 conflict, 429 rate limit)
                            stats.acquire_contention.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => { stats.error_count.fetch_add(1, Ordering::Relaxed); }
                }
            }
            Err(_) => { stats.error_count.fetch_add(1, Ordering::Relaxed); }
        }
    }
}

async fn live_display(stats: Arc<Stats>, duration: u64, concurrency: u64, num_locks: u64) {
    let mut tick = interval(Duration::from_secs(1));
    
    // Print initial header
    println!();
    for _ in 0..12 { println!(); }

    while stats.is_running() {
        select! {
            _ = tick.tick() => {
                let elapsed = stats.start_time.elapsed().as_secs();
                if elapsed > duration { break; }

                let total = stats.total_ops.load(Ordering::Relaxed);
                let won = stats.acquire_success.load(Ordering::Relaxed);
                let lost = stats.acquire_contention.load(Ordering::Relaxed);
                let statuses = stats.status_count.load(Ordering::Relaxed);
                let renew_ok = stats.renew_success.load(Ordering::Relaxed);
                let renew_fail = stats.renew_fail.load(Ordering::Relaxed);
                let release_ok = stats.release_success.load(Ordering::Relaxed);
                let release_fail = stats.release_fail.load(Ordering::Relaxed);
                let errors = stats.error_count.load(Ordering::Relaxed);
                let ops = if elapsed > 0 { total as f64 / elapsed as f64 } else { 0.0 };

                let pct = (elapsed as f64 / duration as f64).min(1.0);
                let filled = (40.0 * pct) as usize;
                let bar = format!("{}{}", "━".repeat(filled), "─".repeat(40 - filled));

                // Move up and overwrite
                print!("\x1b[12A");
                println!("\x1b[K\x1b[1m🐙 OctoStore Stress Test — {} workers fighting over {} locks for {}s\x1b[0m", concurrency, num_locks, duration);
                println!("\x1b[K{}", bar);
                println!("\x1b[K ⏱  Elapsed: {}s / {}s    ({:.0} ops/sec)", elapsed, duration, ops);
                println!("\x1b[K");
                println!("\x1b[K \x1b[32m🔒 Acquires won:     {:>6}\x1b[0m   (got the lock)", won);
                println!("\x1b[K \x1b[33m⚔️  Acquires lost:    {:>6}\x1b[0m   (contention — someone else has it)", lost);
                println!("\x1b[K \x1b[34m👀 Status checks:    {:>6}\x1b[0m", statuses);
                println!("\x1b[K \x1b[35m🔄 Renews:           {:>6} ok / {} fail\x1b[0m", renew_ok, renew_fail);
                println!("\x1b[K \x1b[36m🔓 Releases:         {:>6} ok / {} fail\x1b[0m", release_ok, release_fail);
                let ecolor = if errors > 0 { "\x1b[31m" } else { "\x1b[32m" };
                println!("\x1b[K {}❌ Network errors:   {:>6}\x1b[0m", ecolor, errors);
                println!("\x1b[K \x1b[1m📊 Total operations: {:>6}\x1b[0m", total);
                println!("\x1b[K");
            }
            _ = signal::ctrl_c() => { stats.stop(); break; }
        }
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

    let base_url = args.url.trim_end_matches('/').to_string();
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(args.concurrency as usize)
        .build()
        .expect("Failed to create HTTP client");

    let stats = Stats::new();

    // Start live display
    let stats_c = Arc::clone(&stats);
    let display = tokio::spawn(live_display(stats_c, args.duration, args.concurrency, args.locks));

    // Set up Ctrl+C to stop immediately
    let stats_sigint = Arc::clone(&stats);
    tokio::spawn(async move {
        let _ = signal::ctrl_c().await;
        stats_sigint.stop();
        // Give workers 1 second to finish, then force exit
        sleep(Duration::from_secs(1)).await;
        std::process::exit(0);
    });

    // Launch workers
    let mut handles = Vec::new();
    for wid in 0..args.concurrency {
        let handle = tokio::spawn(stress_worker(
            wid,
            client.clone(),
            base_url.clone(),
            args.token.clone(),
            args.locks,
            Arc::clone(&stats),
        ));
        handles.push(handle);
    }

    // Run for duration
    sleep(Duration::from_secs(args.duration)).await;
    stats.stop();

    for h in handles { let _ = h.await; }
    display.abort();

    // Final report
    let dur = stats.start_time.elapsed().as_secs_f64();
    let total = stats.total_ops.load(Ordering::Relaxed);
    let won = stats.acquire_success.load(Ordering::Relaxed);
    let lost = stats.acquire_contention.load(Ordering::Relaxed);
    let statuses = stats.status_count.load(Ordering::Relaxed);
    let renew_ok = stats.renew_success.load(Ordering::Relaxed);
    let renew_fail = stats.renew_fail.load(Ordering::Relaxed);
    let release_ok = stats.release_success.load(Ordering::Relaxed);
    let release_fail = stats.release_fail.load(Ordering::Relaxed);
    let errors = stats.error_count.load(Ordering::Relaxed);

    let (acq_avg, acq_p50, acq_p95, acq_p99, acq_max) = percentiles(stats.acquire_latencies.lock().unwrap().clone());
    let (st_avg, st_p50, st_p95, st_p99, st_max) = percentiles(stats.status_latencies.lock().unwrap().clone());
    let (rn_avg, rn_p50, rn_p95, rn_p99, rn_max) = percentiles(stats.renew_latencies.lock().unwrap().clone());
    let (rl_avg, rl_p50, rl_p95, rl_p99, rl_max) = percentiles(stats.release_latencies.lock().unwrap().clone());

    println!("\n\x1b[1m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1m🏁 Stress Test Complete\x1b[0m\n");
    println!("  Duration:       {:.1}s", dur);
    println!("  Workers:        {}", args.concurrency);
    println!("  Shared locks:   {}", args.locks);
    println!("  Total ops:      {} ({:.0} ops/sec)", total, total as f64 / dur);
    println!();
    println!("  \x1b[1mContention\x1b[0m");
    println!("  ├─ Acquires won:   {} ({:.1}%)", won, if won+lost > 0 { won as f64 / (won+lost) as f64 * 100.0 } else { 0.0 });
    println!("  └─ Acquires lost:  {} ({:.1}%)", lost, if won+lost > 0 { lost as f64 / (won+lost) as f64 * 100.0 } else { 0.0 });
    println!();
    println!("  \x1b[1mLatencies\x1b[0m");
    println!("  Operation     Count    Avg      p50      p95      p99      Max");
    println!("  ──────────────────────────────────────────────────────────────────");
    println!("  Acquire       {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", won+lost, acq_avg, acq_p50, acq_p95, acq_p99, acq_max);
    println!("  Status        {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", statuses, st_avg, st_p50, st_p95, st_p99, st_max);
    println!("  Renew         {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", renew_ok, rn_avg, rn_p50, rn_p95, rn_p99, rn_max);
    println!("  Release       {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", release_ok, rl_avg, rl_p50, rl_p95, rl_p99, rl_max);
    println!();
    if errors > 0 {
        println!("  \x1b[31m❌ Network errors: {}\x1b[0m", errors);
    } else {
        println!("  \x1b[32m✅ Zero network errors\x1b[0m");
    }
    if renew_fail > 0 || release_fail > 0 {
        println!("  ⚠️  Renew failures: {}, Release failures: {} (race conditions — expected under contention)", renew_fail, release_fail);
    }
    println!("  \x1b[1m🔥 Throughput: {:.0} ops/sec\x1b[0m", total as f64 / dur);
    println!();
}
