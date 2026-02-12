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

    /// Number of locks to cycle through per worker
    #[arg(long, default_value_t = 5)]
    locks_per_worker: u64,

    /// Admin key (required to prevent abuse)
    #[arg(long)]
    admin_key: String,
}

struct Stats {
    acquire_ok: AtomicU64,
    acquire_fail: AtomicU64,
    acquire_latencies: Mutex<Vec<u128>>,
    status_ok: AtomicU64,
    status_latencies: Mutex<Vec<u128>>,
    renew_ok: AtomicU64,
    renew_fail: AtomicU64,
    renew_latencies: Mutex<Vec<u128>>,
    release_ok: AtomicU64,
    release_fail: AtomicU64,
    release_latencies: Mutex<Vec<u128>>,
    error_count: AtomicU64,
    total_ops: AtomicU64,
    start_time: Instant,
    running: AtomicBool,
}

impl Stats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            acquire_ok: AtomicU64::new(0),
            acquire_fail: AtomicU64::new(0),
            acquire_latencies: Mutex::new(Vec::new()),
            status_ok: AtomicU64::new(0),
            status_latencies: Mutex::new(Vec::new()),
            renew_ok: AtomicU64::new(0),
            renew_fail: AtomicU64::new(0),
            renew_latencies: Mutex::new(Vec::new()),
            release_ok: AtomicU64::new(0),
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

/// Each worker owns its own set of locks and hammers the full lifecycle:
/// acquire → status → renew → release → repeat with next lock.
/// This tests raw throughput and correctness of the full lock lifecycle.
/// With many workers, we stress concurrency on the server (DashMap, SQLite, etc.)
async fn throughput_worker(
    worker_id: u64,
    client: Client,
    base_url: String,
    token: String,
    locks_per_worker: u64,
    stats: Arc<Stats>,
) {
    let mut cycle = 0u64;

    while stats.is_running() {
        let lock_name = format!("w{}-l{}", worker_id, cycle % locks_per_worker);
        cycle += 1;

        // ACQUIRE
        let start = Instant::now();
        let result = client
            .post(&format!("{}/locks/{}/acquire", base_url, lock_name))
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({"ttl_seconds": 30}))
            .send().await;
        let lat = start.elapsed().as_millis();
        stats.total_ops.fetch_add(1, Ordering::Relaxed);

        let lease_id = match result {
            Ok(resp) => {
                if resp.status().as_u16() == 401 {
                    eprintln!("\n\x1b[31m❌ Auth failed! Sign in: GET {}/auth/github\x1b[0m", base_url);
                    stats.stop();
                    return;
                }
                stats.acquire_latencies.lock().unwrap().push(lat);
                if resp.status().is_success() {
                    match resp.json::<serde_json::Value>().await {
                        Ok(json) => {
                            stats.acquire_ok.fetch_add(1, Ordering::Relaxed);
                            json.get("lease_id").and_then(|s| s.as_str()).map(|s| s.to_string())
                        }
                        Err(_) => { stats.acquire_fail.fetch_add(1, Ordering::Relaxed); None }
                    }
                } else {
                    // Could be 409 (limit), 422 (validation), etc
                    let _ = resp.text().await; // consume body
                    stats.acquire_fail.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
            Err(_) => { stats.error_count.fetch_add(1, Ordering::Relaxed); None }
        };

        let lease_id = match lease_id {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };

        if !stats.is_running() { break; }

        // STATUS
        let start = Instant::now();
        if let Ok(resp) = client
            .get(&format!("{}/locks/{}", base_url, lock_name))
            .header("Authorization", format!("Bearer {}", token))
            .send().await
        {
            let lat = start.elapsed().as_millis();
            stats.total_ops.fetch_add(1, Ordering::Relaxed);
            if resp.status().is_success() {
                stats.status_ok.fetch_add(1, Ordering::Relaxed);
                stats.status_latencies.lock().unwrap().push(lat);
            }
            let _ = resp.text().await;
        }

        if !stats.is_running() { break; }

        // RENEW
        let start = Instant::now();
        if let Ok(resp) = client
            .post(&format!("{}/locks/{}/renew", base_url, lock_name))
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({"lease_id": lease_id, "ttl_seconds": 30}))
            .send().await
        {
            let lat = start.elapsed().as_millis();
            stats.total_ops.fetch_add(1, Ordering::Relaxed);
            if resp.status().is_success() {
                stats.renew_ok.fetch_add(1, Ordering::Relaxed);
                stats.renew_latencies.lock().unwrap().push(lat);
            } else {
                stats.renew_fail.fetch_add(1, Ordering::Relaxed);
            }
            let _ = resp.text().await;
        }

        if !stats.is_running() { break; }

        // RELEASE
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
                stats.release_ok.fetch_add(1, Ordering::Relaxed);
                stats.release_latencies.lock().unwrap().push(lat);
            } else {
                stats.release_fail.fetch_add(1, Ordering::Relaxed);
            }
            let _ = resp.text().await;
        }
    }
}

async fn live_display(stats: Arc<Stats>, duration: u64, concurrency: u64) {
    let mut tick = interval(Duration::from_secs(1));
    for _ in 0..11 { println!(); }

    while stats.is_running() {
        select! {
            _ = tick.tick() => {
                let elapsed = stats.start_time.elapsed().as_secs();
                if elapsed > duration { break; }

                let total = stats.total_ops.load(Ordering::Relaxed);
                let acq_ok = stats.acquire_ok.load(Ordering::Relaxed);
                let acq_fail = stats.acquire_fail.load(Ordering::Relaxed);
                let st = stats.status_ok.load(Ordering::Relaxed);
                let rn_ok = stats.renew_ok.load(Ordering::Relaxed);
                let rn_fail = stats.renew_fail.load(Ordering::Relaxed);
                let rl_ok = stats.release_ok.load(Ordering::Relaxed);
                let rl_fail = stats.release_fail.load(Ordering::Relaxed);
                let errors = stats.error_count.load(Ordering::Relaxed);
                let ops = if elapsed > 0 { total as f64 / elapsed as f64 } else { 0.0 };

                let pct = (elapsed as f64 / duration as f64).min(1.0);
                let filled = (40.0 * pct) as usize;
                let bar = format!("{}{}", "━".repeat(filled), "─".repeat(40 - filled));

                print!("\x1b[11A");
                println!("\x1b[K\x1b[1m🐙 OctoStore Stress Test — {} workers, {}s\x1b[0m", concurrency, duration);
                println!("\x1b[K{}", bar);
                println!("\x1b[K ⏱  {}s / {}s    \x1b[1m{:.0} ops/sec\x1b[0m", elapsed, duration, ops);
                println!("\x1b[K");
                println!("\x1b[K \x1b[32m🔒 Acquire:  {:>6} ok  {:>6} fail\x1b[0m", acq_ok, acq_fail);
                println!("\x1b[K \x1b[34m👀 Status:   {:>6}\x1b[0m", st);
                println!("\x1b[K \x1b[35m🔄 Renew:    {:>6} ok  {:>6} fail\x1b[0m", rn_ok, rn_fail);
                println!("\x1b[K \x1b[36m🔓 Release:  {:>6} ok  {:>6} fail\x1b[0m", rl_ok, rl_fail);
                let ec = if errors > 0 { "\x1b[31m" } else { "\x1b[32m" };
                println!("\x1b[K {}❌ Errors:   {:>6}\x1b[0m", ec, errors);
                println!("\x1b[K \x1b[1m📊 Total:    {:>6}\x1b[0m", total);
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

    // Validate token first
    let base_url = args.url.trim_end_matches('/').to_string();
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(args.concurrency as usize)
        .build()
        .expect("Failed to create HTTP client");

    print!("🔑 Validating token... ");
    match client.get(&format!("{}/locks", base_url))
        .header("Authorization", format!("Bearer {}", args.token))
        .send().await
    {
        Ok(resp) if resp.status().as_u16() == 401 => {
            println!("\x1b[31mFAILED\x1b[0m");
            eprintln!("   Token is invalid. Sign in first: {}/auth/github", base_url);
            std::process::exit(1);
        }
        Ok(resp) if resp.status().is_success() => {
            println!("\x1b[32mOK\x1b[0m");
        }
        Ok(resp) => {
            println!("\x1b[33mUnexpected status: {}\x1b[0m", resp.status());
        }
        Err(e) => {
            println!("\x1b[31mFailed to connect: {}\x1b[0m", e);
            std::process::exit(1);
        }
    }

    // Check lock limit: concurrency * locks_per_worker must be <= 100
    let total_locks = args.concurrency * args.locks_per_worker;
    if total_locks > 100 {
        eprintln!("\x1b[33m⚠️  {} workers × {} locks/worker = {} locks (max 100 per account)\x1b[0m", 
            args.concurrency, args.locks_per_worker, total_locks);
        eprintln!("   Reducing locks_per_worker to {}", 100 / args.concurrency);
    }
    let effective_locks = if total_locks > 100 { 
        std::cmp::max(1, 100 / args.concurrency)
    } else { 
        args.locks_per_worker 
    };

    let stats = Stats::new();

    // Ctrl+C handler — force exit after 1s
    let stats_sigint = Arc::clone(&stats);
    tokio::spawn(async move {
        let _ = signal::ctrl_c().await;
        stats_sigint.stop();
        sleep(Duration::from_secs(1)).await;
        std::process::exit(0);
    });

    // Start display
    let stats_d = Arc::clone(&stats);
    let display = tokio::spawn(live_display(stats_d, args.duration, args.concurrency));

    // Launch workers
    let mut handles = Vec::new();
    for wid in 0..args.concurrency {
        let handle = tokio::spawn(throughput_worker(
            wid, client.clone(), base_url.clone(), args.token.clone(),
            effective_locks, Arc::clone(&stats),
        ));
        handles.push(handle);
    }

    sleep(Duration::from_secs(args.duration)).await;
    stats.stop();
    for h in handles { let _ = h.await; }
    display.abort();

    // Final report
    let dur = stats.start_time.elapsed().as_secs_f64();
    let total = stats.total_ops.load(Ordering::Relaxed);
    let acq_ok = stats.acquire_ok.load(Ordering::Relaxed);
    let acq_fail = stats.acquire_fail.load(Ordering::Relaxed);
    let st = stats.status_ok.load(Ordering::Relaxed);
    let rn_ok = stats.renew_ok.load(Ordering::Relaxed);
    let rn_fail = stats.renew_fail.load(Ordering::Relaxed);
    let rl_ok = stats.release_ok.load(Ordering::Relaxed);
    let rl_fail = stats.release_fail.load(Ordering::Relaxed);
    let errors = stats.error_count.load(Ordering::Relaxed);

    let (a_avg, a_p50, a_p95, a_p99, a_max) = percentiles(stats.acquire_latencies.lock().unwrap().clone());
    let (s_avg, s_p50, s_p95, s_p99, s_max) = percentiles(stats.status_latencies.lock().unwrap().clone());
    let (n_avg, n_p50, n_p95, n_p99, n_max) = percentiles(stats.renew_latencies.lock().unwrap().clone());
    let (r_avg, r_p50, r_p95, r_p99, r_max) = percentiles(stats.release_latencies.lock().unwrap().clone());

    println!("\n\x1b[1m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1m🏁 Stress Test Complete\x1b[0m\n");
    println!("  Duration:       {:.1}s", dur);
    println!("  Workers:        {}", args.concurrency);
    println!("  Locks/worker:   {}", effective_locks);
    println!("  Total ops:      {} (\x1b[1m{:.0} ops/sec\x1b[0m)\n", total, total as f64 / dur);

    println!("  \x1b[1mResults\x1b[0m");
    println!("  Acquire:  {} ok / {} fail", acq_ok, acq_fail);
    println!("  Status:   {} ok", st);
    println!("  Renew:    {} ok / {} fail", rn_ok, rn_fail);
    println!("  Release:  {} ok / {} fail\n", rl_ok, rl_fail);

    println!("  \x1b[1mLatencies (successful ops only)\x1b[0m");
    println!("  Operation     Count    Avg      p50      p95      p99      Max");
    println!("  ──────────────────────────────────────────────────────────────────");
    println!("  Acquire       {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", acq_ok, a_avg, a_p50, a_p95, a_p99, a_max);
    println!("  Status        {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", st, s_avg, s_p50, s_p95, s_p99, s_max);
    println!("  Renew         {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", rn_ok, n_avg, n_p50, n_p95, n_p99, n_max);
    println!("  Release       {:>5}  {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>6.1}ms {:>7.1}ms", rl_ok, r_avg, r_p50, r_p95, r_p99, r_max);

    println!();
    if errors > 0 { println!("  \x1b[31m❌ Network errors: {}\x1b[0m", errors); }
    else { println!("  \x1b[32m✅ Zero network errors\x1b[0m"); }
    
    let fail_total = acq_fail + rn_fail + rl_fail;
    let success_total = acq_ok + st + rn_ok + rl_ok;
    let success_rate = if success_total + fail_total > 0 { 
        success_total as f64 / (success_total + fail_total) as f64 * 100.0 
    } else { 0.0 };
    println!("  📈 Success rate: {:.1}%", success_rate);
    println!("  \x1b[1m🔥 Throughput: {:.0} ops/sec\x1b[0m\n", total as f64 / dur);
}
