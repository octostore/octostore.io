use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use serde_json::{json, Value};

pub struct EndpointMetrics {
    pub count: AtomicU64,
    pub total_duration_ms: AtomicU64,
    pub errors: AtomicU64,
    pub min_ms: AtomicU32,
    pub max_ms: AtomicU32,
    // Simple histogram with fixed buckets: [0, 0.5, 1, 2, 5, 10, 50, 100]ms
    pub bucket_0_5: AtomicU64,
    pub bucket_1: AtomicU64,
    pub bucket_2: AtomicU64,
    pub bucket_5: AtomicU64,
    pub bucket_10: AtomicU64,
    pub bucket_50: AtomicU64,
    pub bucket_100: AtomicU64,
    pub bucket_inf: AtomicU64,
}

impl Default for EndpointMetrics {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_duration_ms: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            min_ms: AtomicU32::new(u32::MAX),
            max_ms: AtomicU32::new(0),
            bucket_0_5: AtomicU64::new(0),
            bucket_1: AtomicU64::new(0),
            bucket_2: AtomicU64::new(0),
            bucket_5: AtomicU64::new(0),
            bucket_10: AtomicU64::new(0),
            bucket_50: AtomicU64::new(0),
            bucket_100: AtomicU64::new(0),
            bucket_inf: AtomicU64::new(0),
        }
    }
}

impl EndpointMetrics {
    pub fn record(&self, duration_ms: f64, is_error: bool) {
        let duration_ms_u32 = duration_ms as u32;
        let duration_ms_u64 = duration_ms as u64;
        
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_duration_ms.fetch_add(duration_ms_u64, Ordering::Relaxed);
        
        if is_error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        
        // Update min
        self.min_ms.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            if current == u32::MAX || duration_ms_u32 < current {
                Some(duration_ms_u32)
            } else {
                None
            }
        }).ok();
        
        // Update max
        self.max_ms.fetch_max(duration_ms_u32, Ordering::Relaxed);
        
        // Update histogram buckets
        if duration_ms <= 0.5 {
            self.bucket_0_5.fetch_add(1, Ordering::Relaxed);
        } else if duration_ms <= 1.0 {
            self.bucket_1.fetch_add(1, Ordering::Relaxed);
        } else if duration_ms <= 2.0 {
            self.bucket_2.fetch_add(1, Ordering::Relaxed);
        } else if duration_ms <= 5.0 {
            self.bucket_5.fetch_add(1, Ordering::Relaxed);
        } else if duration_ms <= 10.0 {
            self.bucket_10.fetch_add(1, Ordering::Relaxed);
        } else if duration_ms <= 50.0 {
            self.bucket_50.fetch_add(1, Ordering::Relaxed);
        } else if duration_ms <= 100.0 {
            self.bucket_100.fetch_add(1, Ordering::Relaxed);
        } else {
            self.bucket_inf.fetch_add(1, Ordering::Relaxed);
        }
    }
    
    pub fn snapshot(&self) -> EndpointSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let total_duration_ms = self.total_duration_ms.load(Ordering::Relaxed);
        let errors = self.errors.load(Ordering::Relaxed);
        let min_ms = self.min_ms.load(Ordering::Relaxed);
        let max_ms = self.max_ms.load(Ordering::Relaxed);
        
        // Calculate percentiles from histogram
        let bucket_counts = vec![
            self.bucket_0_5.load(Ordering::Relaxed),
            self.bucket_1.load(Ordering::Relaxed),
            self.bucket_2.load(Ordering::Relaxed),
            self.bucket_5.load(Ordering::Relaxed),
            self.bucket_10.load(Ordering::Relaxed),
            self.bucket_50.load(Ordering::Relaxed),
            self.bucket_100.load(Ordering::Relaxed),
            self.bucket_inf.load(Ordering::Relaxed),
        ];
        
        let (p50_ms, p95_ms, p99_ms) = calculate_percentiles(&bucket_counts, count);
        
        EndpointSnapshot {
            count,
            avg_ms: if count > 0 { total_duration_ms as f64 / count as f64 } else { 0.0 },
            p50_ms,
            p95_ms,
            p99_ms,
            errors,
            min_ms: if min_ms == u32::MAX { 0 } else { min_ms },
            max_ms,
        }
    }
}

pub struct Metrics {
    pub start_time: Instant,
    pub acquire: EndpointMetrics,
    pub release: EndpointMetrics,
    pub renew: EndpointMetrics,
    pub status: EndpointMetrics,
    pub list: EndpointMetrics,
    pub auth: EndpointMetrics,
    pub total_requests: AtomicU64,
    pub lock_store_acquires: AtomicU64,
    pub lock_store_releases: AtomicU64,
    pub lock_store_expirations: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            start_time: Instant::now(),
            acquire: EndpointMetrics::default(),
            release: EndpointMetrics::default(),
            renew: EndpointMetrics::default(),
            status: EndpointMetrics::default(),
            list: EndpointMetrics::default(),
            auth: EndpointMetrics::default(),
            total_requests: AtomicU64::new(0),
            lock_store_acquires: AtomicU64::new(0),
            lock_store_releases: AtomicU64::new(0),
            lock_store_expirations: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }
}

#[derive(Debug)]
pub struct EndpointSnapshot {
    pub count: u64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub errors: u64,
    pub min_ms: u32,
    pub max_ms: u32,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    
    pub fn record_request(&self, endpoint: &str, duration_ms: f64, is_error: bool) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        
        let endpoint_metrics = match endpoint {
            "acquire" => &self.acquire,
            "release" => &self.release,
            "renew" => &self.renew,
            "status" => &self.status,
            "list" => &self.list,
            "auth" => &self.auth,
            _ => return, // Unknown endpoint
        };
        
        endpoint_metrics.record(duration_ms, is_error);
    }
    
    pub fn record_lock_operation(&self, operation: &str) {
        match operation {
            "acquire" => { self.lock_store_acquires.fetch_add(1, Ordering::Relaxed); }
            "release" => { self.lock_store_releases.fetch_add(1, Ordering::Relaxed); }
            "expiration" => { self.lock_store_expirations.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }
    }
    
    pub fn record_cache_hit(&self, hit: bool) {
        if hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
    }
    
    pub fn snapshot(&self) -> Value {
        let uptime = self.start_time.elapsed().as_secs();
        let total_requests = self.total_requests.load(Ordering::Relaxed);
        let requests_per_second = if uptime > 0 { 
            total_requests as f64 / uptime as f64 
        } else { 
            0.0 
        };
        
        // Get system memory usage (approximation)
        let memory_bytes = get_memory_usage();
        
        json!({
            "uptime_seconds": uptime,
            "total_requests": total_requests,
            "requests_per_second": requests_per_second,
            "active_locks": 0, // Will be filled by caller
            "total_users": 0,  // Will be filled by caller
            "endpoints": {
                "acquire": endpoint_to_json(&self.acquire.snapshot()),
                "release": endpoint_to_json(&self.release.snapshot()),
                "renew": endpoint_to_json(&self.renew.snapshot()),
                "status": endpoint_to_json(&self.status.snapshot()),
                "list": endpoint_to_json(&self.list.snapshot()),
                "auth": endpoint_to_json(&self.auth.snapshot()),
            },
            "lock_store": {
                "total_acquires": self.lock_store_acquires.load(Ordering::Relaxed),
                "total_releases": self.lock_store_releases.load(Ordering::Relaxed),
                "total_expirations": self.lock_store_expirations.load(Ordering::Relaxed),
                "cache_hits": self.cache_hits.load(Ordering::Relaxed),
                "cache_misses": self.cache_misses.load(Ordering::Relaxed),
            },
            "memory_bytes": memory_bytes
        })
    }
}

fn endpoint_to_json(snapshot: &EndpointSnapshot) -> Value {
    json!({
        "count": snapshot.count,
        "avg_ms": format!("{:.1}", snapshot.avg_ms),
        "p50_ms": format!("{:.1}", snapshot.p50_ms),
        "p95_ms": format!("{:.1}", snapshot.p95_ms),
        "p99_ms": format!("{:.1}", snapshot.p99_ms),
        "errors": snapshot.errors,
        "min_ms": snapshot.min_ms,
        "max_ms": snapshot.max_ms
    })
}

fn calculate_percentiles(bucket_counts: &[u64], total: u64) -> (f64, f64, f64) {
    if total == 0 {
        return (0.0, 0.0, 0.0);
    }
    
    let bucket_bounds = [0.5, 1.0, 2.0, 5.0, 10.0, 50.0, 100.0, f64::INFINITY];
    let p50_target = (total as f64 * 0.50) as u64;
    let p95_target = (total as f64 * 0.95) as u64;
    let p99_target = (total as f64 * 0.99) as u64;
    
    let mut cumulative = 0;
    let mut p50_ms = 0.0;
    let mut p95_ms = 0.0;
    let mut p99_ms = 0.0;
    
    for (i, &count) in bucket_counts.iter().enumerate() {
        let _prev_cumulative = cumulative;
        cumulative += count;
        
        // Check if percentiles fall in this bucket
        if p50_ms == 0.0 && cumulative >= p50_target {
            p50_ms = bucket_bounds[i];
        }
        if p95_ms == 0.0 && cumulative >= p95_target {
            p95_ms = bucket_bounds[i];
        }
        if p99_ms == 0.0 && cumulative >= p99_target {
            p99_ms = bucket_bounds[i];
        }
    }
    
    (p50_ms, p95_ms, p99_ms)
}

fn get_memory_usage() -> u64 {
    // Simple approximation - read from /proc/self/status
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| {
                    line.split_whitespace()
                        .nth(1)
                        .and_then(|kb| kb.parse::<u64>().ok())
                        .map(|kb| kb * 1024) // Convert KB to bytes
                })
        })
        .unwrap_or(0)
}

pub fn endpoint_from_path(path: &str) -> Option<&'static str> {
    if path.contains("/locks/") && path.contains("/acquire") {
        Some("acquire")
    } else if path.contains("/locks/") && path.contains("/release") {
        Some("release")
    } else if path.contains("/locks/") && path.contains("/renew") {
        Some("renew")
    } else if path.starts_with("/locks/") && !path.contains("/acquire") && !path.contains("/release") && !path.contains("/renew") {
        Some("status")
    } else if path == "/locks" {
        Some("list")
    } else if path.starts_with("/auth/") {
        Some("auth")
    } else {
        None
    }
}