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
    time::sleep,
    signal,
};

#[derive(Parser, Debug)]
#[command(name = "octostore-bench")]
#[command(about = "OctoStore Wave Stress Test")]
struct Args {
    /// Base URL
    #[arg(long, default_value = "https://api.octostore.io")]
    url: String,

    /// Bearer token (REQUIRED)
    #[arg(long)]
    token: String,

    /// Seconds per wave
    #[arg(long, default_value_t = 10)]
    wave_duration: u64,

    /// Admin key
    #[arg(long)]
    admin_key: String,
}

struct WaveStats {
    acquire_ok: AtomicU64,
    acquire_held: AtomicU64,     // contention: someone else has it
    acquire_limit: AtomicU64,    // rejected: 100-lock limit hit
    acquire_latencies: Mutex<Vec<u128>>,
    status_ok: AtomicU64,
    status_latencies: Mutex<Vec<u128>>,
    renew_ok: AtomicU64,
    renew_fail: AtomicU64,
    renew_latencies: Mutex<Vec<u128>>,
    release_ok: AtomicU64,
    release_fail: AtomicU64,
    release_latencies: Mutex<Vec<u128>>,
    errors: AtomicU64,
    total_ops: AtomicU64,
    running: AtomicBool,
}

impl WaveStats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            acquire_ok: AtomicU64::new(0),
            acquire_held: AtomicU64::new(0),
            acquire_limit: AtomicU64::new(0),
            acquire_latencies: Mutex::new(Vec::new()),
            status_ok: AtomicU64::new(0),
            status_latencies: Mutex::new(Vec::new()),
            renew_ok: AtomicU64::new(0),
            renew_fail: AtomicU64::new(0),
            renew_latencies: Mutex::new(Vec::new()),
            release_ok: AtomicU64::new(0),
            release_fail: AtomicU64::new(0),
            release_latencies: Mutex::new(Vec::new()),
            errors: AtomicU64::new(0),
            total_ops: AtomicU64::new(0),
            running: AtomicBool::new(true),
        })
    }
    fn stop(&self) { self.running.store(false, Ordering::Relaxed); }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }
}

fn percentiles(mut v: Vec<u128>) -> (f64, f64, f64, f64) {
    if v.is_empty() { return (0.0, 0.0, 0.0, 0.0); }
    v.sort_unstable();
    let n = v.len();
    (v.iter().sum::<u128>() as f64 / n as f64, 
     v[n*50/100] as f64, v[n*95/100] as f64, v[n-1] as f64)
}

/// Worker that owns one lock and hammers acquire→status→renew→release cycle
async fn worker(
    lock_name: String,
    client: Client,
    base_url: String,
    token: String,
    stats: Arc<WaveStats>,
) {
    while stats.is_running() {
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
                let status_code = resp.status().as_u16();
                stats.acquire_latencies.lock().unwrap().push(lat);
                
                if status_code == 401 {
                    eprintln!("\n\x1b[31m❌ Auth failed!\x1b[0m");
                    stats.stop();
                    return;
                }
                
                match resp.json::<serde_json::Value>().await {
                    Ok(json) => {
                        if status_code == 200 || status_code == 201 {
                            let s = json.get("status").and_then(|s| s.as_str()).unwrap_or("");
                            if s == "acquired" {
                                stats.acquire_ok.fetch_add(1, Ordering::Relaxed);
                                json.get("lease_id").and_then(|s| s.as_str()).map(|s| s.to_string())
                            } else {
                                // "held" by someone else
                                stats.acquire_held.fetch_add(1, Ordering::Relaxed);
                                None
                            }
                        } else if status_code == 409 || status_code == 422 {
                            // Lock limit exceeded or validation error
                            let err = json.get("error").and_then(|s| s.as_str()).unwrap_or("");
                            if err.contains("limit") || err.contains("100") || err.contains("maximum") {
                                stats.acquire_limit.fetch_add(1, Ordering::Relaxed);
                            } else {
                                stats.acquire_held.fetch_add(1, Ordering::Relaxed);
                            }
                            None
                        } else {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            None
                        }
                    }
                    Err(_) => { stats.errors.fetch_add(1, Ordering::Relaxed); None }
                }
            }
            Err(_) => { stats.errors.fetch_add(1, Ordering::Relaxed); None }
        };

        let lease_id = match lease_id {
            Some(id) if !id.is_empty() => id,
            _ => { 
                // Brief pause before retrying on failure
                sleep(Duration::from_millis(10)).await;
                continue; 
            }
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

/// Clean up all locks from a wave
async fn cleanup_locks(client: &Client, base_url: &str, token: &str, lock_names: &[String]) {
    for name in lock_names {
        // Try to release with dummy lease (will fail but that's fine — just want to clear)
        let _ = client
            .post(&format!("{}/locks/{}/release", base_url, name))
            .header("Authorization", format!("Bearer {}", token))
            .json(&json!({"lease_id": "00000000-0000-0000-0000-000000000000"}))
            .send().await;
    }
    // Give server a moment to process
    sleep(Duration::from_millis(500)).await;
}

#[allow(dead_code)]
struct WaveResult {
    workers: u64,
    duration_secs: f64,
    total_ops: u64,
    acquire_ok: u64,
    acquire_held: u64,
    acquire_limit: u64,
    status_ok: u64,
    renew_ok: u64,
    renew_fail: u64,
    release_ok: u64,
    release_fail: u64,
    errors: u64,
    acquire_p50: f64,
    acquire_p95: f64,
    ops_per_sec: f64,
}

async fn run_wave(
    wave_num: usize,
    num_workers: u64,
    wave_duration: u64,
    client: &Client,
    base_url: &str,
    token: &str,
) -> WaveResult {
    let expect_failures = num_workers > 100;
    let emoji = if expect_failures { "💥" } else { "🌊" };
    
    println!("\n{} \x1b[1mWave {} — {} workers{}\x1b[0m",
        emoji, wave_num, num_workers,
        if expect_failures { " (exceeds 100-lock limit!)" } else { "" }
    );
    println!("  {}", "─".repeat(50));

    let stats = WaveStats::new();
    
    // Generate lock names for this wave
    let lock_names: Vec<String> = (0..num_workers)
        .map(|i| format!("wave{}-w{}", wave_num, i))
        .collect();

    // Launch workers
    let mut handles = Vec::new();
    for i in 0..num_workers {
        let handle = tokio::spawn(worker(
            lock_names[i as usize].clone(),
            client.clone(),
            base_url.to_string(),
            token.to_string(),
            Arc::clone(&stats),
        ));
        handles.push(handle);
    }

    // Live progress
    let start = Instant::now();
    loop {
        sleep(Duration::from_secs(1)).await;
        let elapsed = start.elapsed().as_secs();
        let total = stats.total_ops.load(Ordering::Relaxed);
        let ok = stats.acquire_ok.load(Ordering::Relaxed);
        let held = stats.acquire_held.load(Ordering::Relaxed);
        let limit = stats.acquire_limit.load(Ordering::Relaxed);
        let errors = stats.errors.load(Ordering::Relaxed);
        let ops = if elapsed > 0 { total as f64 / elapsed as f64 } else { 0.0 };
        
        print!("\r\x1b[K  ⏱ {}s/{} | {:.0} ops/s | ✅{} ⚔️{} 🚫{} ❌{}",
            elapsed, wave_duration, ops, ok, held, limit, errors);
        
        if elapsed >= wave_duration { break; }
    }
    println!();

    stats.stop();
    for h in handles { let _ = h.await; }

    let dur = start.elapsed().as_secs_f64();
    let total = stats.total_ops.load(Ordering::Relaxed);
    let acq_ok = stats.acquire_ok.load(Ordering::Relaxed);
    let acq_held = stats.acquire_held.load(Ordering::Relaxed);
    let acq_limit = stats.acquire_limit.load(Ordering::Relaxed);
    let st = stats.status_ok.load(Ordering::Relaxed);
    let rn_ok = stats.renew_ok.load(Ordering::Relaxed);
    let rn_fail = stats.renew_fail.load(Ordering::Relaxed);
    let rl_ok = stats.release_ok.load(Ordering::Relaxed);
    let rl_fail = stats.release_fail.load(Ordering::Relaxed);
    let errors = stats.errors.load(Ordering::Relaxed);
    let (_, p50, p95, _) = percentiles(stats.acquire_latencies.lock().unwrap().clone());

    // Print wave summary
    println!("  \x1b[32m✅ Acquired: {}\x1b[0m  \x1b[33m⚔️ Contention: {}\x1b[0m  \x1b[31m🚫 Limit rejected: {}\x1b[0m", 
        acq_ok, acq_held, acq_limit);
    println!("  📊 Status: {} | Renew: {}/{} | Release: {}/{}", 
        st, rn_ok, rn_ok + rn_fail, rl_ok, rl_ok + rl_fail);
    println!("  ⚡ Latency: p50={:.0}ms  p95={:.0}ms  |  {:.0} ops/sec", p50, p95, total as f64 / dur);
    
    if expect_failures && acq_limit > 0 {
        println!("  \x1b[32m✅ Server correctly rejected {} requests over the 100-lock limit\x1b[0m", acq_limit);
    }

    // Cleanup locks before next wave
    print!("  🧹 Cleaning up...");
    cleanup_locks(client, base_url, token, &lock_names).await;
    println!(" done");

    WaveResult {
        workers: num_workers,
        duration_secs: dur,
        total_ops: total,
        acquire_ok: acq_ok,
        acquire_held: acq_held,
        acquire_limit: acq_limit,
        status_ok: st,
        renew_ok: rn_ok,
        renew_fail: rn_fail,
        release_ok: rl_ok,
        release_fail: rl_fail,
        errors,
        acquire_p50: p50,
        acquire_p95: p95,
        ops_per_sec: total as f64 / dur,
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Validate admin key
    let expected_hash = "13478763f37c2e09df7a315a960ec607b69179f58f60b5aa91e3f1c292c77698";
    let mut hasher = Sha256::new();
    hasher.update(args.admin_key.as_bytes());
    if format!("{:x}", hasher.finalize()) != expected_hash {
        println!("❌ Invalid admin key");
        std::process::exit(1);
    }

    let base_url = args.url.trim_end_matches('/').to_string();
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(200)
        .build()
        .expect("Failed to create HTTP client");

    // Ctrl+C → force exit
    tokio::spawn(async move {
        let _ = signal::ctrl_c().await;
        println!("\n\n\x1b[33m⚠️  Interrupted\x1b[0m");
        std::process::exit(0);
    });

    // Validate token
    print!("🔑 Validating token... ");
    match client.get(&format!("{}/locks", base_url))
        .header("Authorization", format!("Bearer {}", args.token))
        .send().await
    {
        Ok(resp) if resp.status().as_u16() == 401 => {
            println!("\x1b[31mFAILED\x1b[0m — sign in: {}/auth/github", base_url);
            std::process::exit(1);
        }
        Ok(_) => println!("\x1b[32mOK\x1b[0m"),
        Err(e) => { println!("\x1b[31mError: {}\x1b[0m", e); std::process::exit(1); }
    }

    println!("\n\x1b[1m🐙 OctoStore Wave Stress Test\x1b[0m");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Target:     {}", base_url);
    println!("  Per wave:   {}s", args.wave_duration);
    println!("  Waves:      1 → 10 → 20 → 50 → 100 → 150 (overflow!)");

    let waves = vec![1, 10, 20, 50, 100, 150];
    let mut results = Vec::new();

    for (i, &workers) in waves.iter().enumerate() {
        let result = run_wave(i + 1, workers, args.wave_duration, &client, &base_url, &args.token).await;
        results.push(result);
    }

    // Final summary table
    println!("\n\x1b[1m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1m🏁 All Waves Complete\x1b[0m\n");
    println!("  Workers   Ops/sec    Acquired   Contention   Limit🚫   p50      p95      Errors");
    println!("  ─────────────────────────────────────────────────────────────────────────────────");
    for r in &results {
        println!("  {:>5}     {:>6.0}     {:>6}     {:>6}       {:>5}     {:>5.0}ms  {:>5.0}ms  {:>5}",
            r.workers, r.ops_per_sec, r.acquire_ok, r.acquire_held, r.acquire_limit,
            r.acquire_p50, r.acquire_p95, r.errors);
    }
    
    let total_ops: u64 = results.iter().map(|r| r.total_ops).sum();
    let total_dur: f64 = results.iter().map(|r| r.duration_secs).sum();
    let limit_rejections: u64 = results.iter().map(|r| r.acquire_limit).sum();
    
    println!();
    println!("  \x1b[1mOverall: {} total ops in {:.0}s ({:.0} avg ops/sec)\x1b[0m", total_ops, total_dur, total_ops as f64 / total_dur);
    if limit_rejections > 0 {
        println!("  \x1b[32m✅ Server correctly enforced 100-lock limit ({} rejections)\x1b[0m", limit_rejections);
    }
    println!();
}
