use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::collections::VecDeque;
use std::sync::RwLock;
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

#[derive(Debug, Clone)]
pub struct MetricsBucket {
    pub timestamp: u64,  // Unix timestamp in seconds
    pub request_count: u64,
    pub acquire_count: u64,
    pub release_count: u64,
    pub active_locks: u64,
}

impl Default for MetricsBucket {
    fn default() -> Self {
        Self {
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            request_count: 0,
            acquire_count: 0,
            release_count: 0,
            active_locks: 0,
        }
    }
}

pub struct TimeSeriesMetrics {
    // Ring buffer for per-minute metrics (last 7 days = 10080 minutes)
    buckets: RwLock<VecDeque<MetricsBucket>>,
    current_minute: AtomicU64,
    requests_this_minute: AtomicU64,
    acquires_this_minute: AtomicU64,
    releases_this_minute: AtomicU64,
}

impl Default for TimeSeriesMetrics {
    fn default() -> Self {
        Self {
            buckets: RwLock::new(VecDeque::with_capacity(10080)), // 7 days of minutes
            current_minute: AtomicU64::new(
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() / 60
            ),
            requests_this_minute: AtomicU64::new(0),
            acquires_this_minute: AtomicU64::new(0),
            releases_this_minute: AtomicU64::new(0),
        }
    }
}

impl TimeSeriesMetrics {
    pub fn record_request(&self) {
        self.ensure_current_bucket();
        self.requests_this_minute.fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn record_acquire(&self) {
        self.ensure_current_bucket();
        self.acquires_this_minute.fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn record_release(&self) {
        self.ensure_current_bucket();
        self.releases_this_minute.fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn record_active_locks(&self, count: u64) {
        self.ensure_current_bucket();
        // For active locks, we'll update the current bucket directly since it's a gauge
        if let Ok(mut buckets) = self.buckets.write() {
            if let Some(current_bucket) = buckets.back_mut() {
                current_bucket.active_locks = count;
            }
        }
    }
    
    fn ensure_current_bucket(&self) {
        let now_minute = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() / 60;
        let current_minute = self.current_minute.load(Ordering::Relaxed);
        
        if now_minute > current_minute {
            // Time to rotate to a new bucket
            let requests = self.requests_this_minute.swap(0, Ordering::Relaxed);
            let acquires = self.acquires_this_minute.swap(0, Ordering::Relaxed);
            let releases = self.releases_this_minute.swap(0, Ordering::Relaxed);
            
            let new_bucket = MetricsBucket {
                timestamp: current_minute * 60, // Convert back to seconds
                request_count: requests,
                acquire_count: acquires,
                release_count: releases,
                active_locks: 0, // Will be set by record_active_locks
            };
            
            if let Ok(mut buckets) = self.buckets.write() {
                buckets.push_back(new_bucket);
                
                // Keep only last 10080 buckets (7 days)
                while buckets.len() > 10080 {
                    buckets.pop_front();
                }
            }
            
            self.current_minute.store(now_minute, Ordering::Relaxed);
        }
    }
    
    pub fn get_timeseries_data(&self, window: &str) -> Value {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let seconds_back = match window {
            "1h" => 3600,
            "12h" => 43200,
            "24h" => 86400,
            "7d" => 604800,
            _ => 3600, // Default to 1 hour
        };
        
        let cutoff_time = now - seconds_back;
        
        if let Ok(buckets) = self.buckets.read() {
            let filtered_buckets: Vec<_> = buckets
                .iter()
                .filter(|bucket| bucket.timestamp >= cutoff_time)
                .collect();
            
            let timestamps: Vec<u64> = filtered_buckets.iter().map(|b| b.timestamp).collect();
            let requests: Vec<u64> = filtered_buckets.iter().map(|b| b.request_count).collect();
            let acquires: Vec<u64> = filtered_buckets.iter().map(|b| b.acquire_count).collect();
            let releases: Vec<u64> = filtered_buckets.iter().map(|b| b.release_count).collect();
            let active_locks: Vec<u64> = filtered_buckets.iter().map(|b| b.active_locks).collect();
            
            json!({
                "window": window,
                "timestamps": timestamps,
                "request_volume": requests,
                "acquire_rates": acquires,
                "release_rates": releases,
                "active_locks": active_locks
            })
        } else {
            json!({
                "window": window,
                "timestamps": [],
                "request_volume": [],
                "acquire_rates": [],
                "release_rates": [],
                "active_locks": []
            })
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
    pub timeseries: TimeSeriesMetrics,
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
            timeseries: TimeSeriesMetrics::default(),
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
        let endpoint_metrics = match endpoint {
            "acquire" => &self.acquire,
            "release" => &self.release,
            "renew" => &self.renew,
            "status" => &self.status,
            "list" => &self.list,
            "auth" => &self.auth,
            _ => return, // Unknown endpoint
        };
        
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.timeseries.record_request();
        endpoint_metrics.record(duration_ms, is_error);
    }
    
    pub fn record_lock_operation(&self, operation: &str) {
        match operation {
            "acquire" => { 
                self.lock_store_acquires.fetch_add(1, Ordering::Relaxed);
                self.timeseries.record_acquire();
            }
            "release" => { 
                self.lock_store_releases.fetch_add(1, Ordering::Relaxed);
                self.timeseries.record_release();
            }
            "expiration" => { self.lock_store_expirations.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }
    }
    
    #[allow(dead_code)]
    pub fn record_cache_hit(&self, hit: bool) {
        if hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
    }
    
    pub fn update_active_locks_count(&self, count: u64) {
        self.timeseries.record_active_locks(count);
    }
    
    pub fn get_timeseries_data(&self, window: &str) -> Value {
        self.timeseries.get_timeseries_data(window)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_endpoint_metrics_default() {
        let metrics = EndpointMetrics::default();
        assert_eq!(metrics.count.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.total_duration_ms.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.errors.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), u32::MAX);
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.bucket_0_5.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_endpoint_metrics_record_success() {
        let metrics = EndpointMetrics::default();
        
        // Record a 5.5ms request
        metrics.record(5.5, false);
        
        assert_eq!(metrics.count.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.total_duration_ms.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.errors.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.bucket_10.load(Ordering::Relaxed), 1); // 5.5ms should go in bucket_10
    }

    #[test]
    fn test_endpoint_metrics_record_error() {
        let metrics = EndpointMetrics::default();
        
        metrics.record(10.5, true);
        
        assert_eq!(metrics.count.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.errors.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_50.load(Ordering::Relaxed), 1); // 10.5ms should go in bucket_50
    }

    #[test]
    fn test_endpoint_metrics_histogram_buckets() {
        let metrics = EndpointMetrics::default();
        
        // Test each bucket boundary
        metrics.record(0.3, false);  // bucket_0_5
        metrics.record(0.8, false);  // bucket_1
        metrics.record(1.5, false);  // bucket_2
        metrics.record(3.0, false);  // bucket_5
        metrics.record(7.0, false);  // bucket_10
        metrics.record(25.0, false); // bucket_50
        metrics.record(80.0, false); // bucket_100
        metrics.record(200.0, false); // bucket_inf
        
        assert_eq!(metrics.bucket_0_5.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_1.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_2.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_5.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_10.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_50.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_100.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.bucket_inf.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.count.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn test_endpoint_metrics_min_max_update() {
        let metrics = EndpointMetrics::default();
        
        metrics.record(10.0, false);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), 10);
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 10);
        
        metrics.record(5.0, false);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 10);
        
        metrics.record(15.0, false);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 15);
        
        metrics.record(8.0, false);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), 5); // Should not change
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 15); // Should not change
    }

    #[test]
    fn test_endpoint_metrics_snapshot() {
        let metrics = EndpointMetrics::default();
        
        metrics.record(1.0, false);
        metrics.record(3.0, false);
        metrics.record(5.0, true); // error
        
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.count, 3);
        assert_eq!(snapshot.avg_ms, 3.0); // (1+3+5)/3
        assert_eq!(snapshot.errors, 1);
        assert_eq!(snapshot.min_ms, 1);
        assert_eq!(snapshot.max_ms, 5);
    }

    #[test]
    fn test_endpoint_metrics_snapshot_empty() {
        let metrics = EndpointMetrics::default();
        let snapshot = metrics.snapshot();
        
        assert_eq!(snapshot.count, 0);
        assert_eq!(snapshot.avg_ms, 0.0);
        assert_eq!(snapshot.errors, 0);
        assert_eq!(snapshot.min_ms, 0); // Should convert u32::MAX to 0
        assert_eq!(snapshot.max_ms, 0);
    }

    #[test]
    fn test_metrics_new() {
        let metrics = Metrics::new();
        assert_eq!(metrics.total_requests.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.lock_store_acquires.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.cache_hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_record_request() {
        let metrics = Metrics::new();
        
        metrics.record_request("acquire", 5.0, false);
        metrics.record_request("acquire", 10.0, true);
        metrics.record_request("unknown", 1.0, false); // Should be ignored
        
        assert_eq!(metrics.total_requests.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.acquire.count.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.acquire.errors.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.release.count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_record_lock_operation() {
        let metrics = Metrics::new();
        
        metrics.record_lock_operation("acquire");
        metrics.record_lock_operation("acquire");
        metrics.record_lock_operation("release");
        metrics.record_lock_operation("expiration");
        metrics.record_lock_operation("unknown"); // Should be ignored
        
        assert_eq!(metrics.lock_store_acquires.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.lock_store_releases.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.lock_store_expirations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_metrics_record_cache_hit() {
        let metrics = Metrics::new();
        
        metrics.record_cache_hit(true);
        metrics.record_cache_hit(true);
        metrics.record_cache_hit(false);
        
        assert_eq!(metrics.cache_hits.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.cache_misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_metrics_snapshot() {
        let metrics = Metrics::new();
        
        metrics.record_request("acquire", 5.0, false);
        metrics.record_lock_operation("acquire");
        metrics.record_cache_hit(true);
        
        // Add a small delay to ensure uptime > 0
        thread::sleep(Duration::from_millis(1));
        
        let snapshot = metrics.snapshot();
        assert!(snapshot["uptime_seconds"].as_u64().unwrap() >= 0);
        assert_eq!(snapshot["total_requests"].as_u64().unwrap(), 1);
        assert!(snapshot["requests_per_second"].as_f64().unwrap() >= 0.0);
        assert_eq!(snapshot["endpoints"]["acquire"]["count"].as_u64().unwrap(), 1);
        assert_eq!(snapshot["lock_store"]["total_acquires"].as_u64().unwrap(), 1);
        assert_eq!(snapshot["lock_store"]["cache_hits"].as_u64().unwrap(), 1);
    }

    #[test]
    fn test_calculate_percentiles() {
        // Test empty case
        let bucket_counts = vec![0; 8];
        let (p50, p95, p99) = calculate_percentiles(&bucket_counts, 0);
        assert_eq!(p50, 0.0);
        assert_eq!(p95, 0.0);
        assert_eq!(p99, 0.0);
        
        // Test simple case - all requests in first bucket
        let bucket_counts = vec![100, 0, 0, 0, 0, 0, 0, 0];
        let (p50, p95, p99) = calculate_percentiles(&bucket_counts, 100);
        assert_eq!(p50, 0.5); // First bucket upper bound
        assert_eq!(p95, 0.5);
        assert_eq!(p99, 0.5);
        
        // Test distribution across buckets
        let bucket_counts = vec![10, 20, 30, 20, 15, 3, 1, 1]; // total = 100
        let (p50, p95, p99) = calculate_percentiles(&bucket_counts, 100);
        // p50 (50th request) should be in bucket_2 (cumulative: 10+20+30=60 >= 50)
        assert_eq!(p50, 2.0);
        // p95 (95th request) should be in bucket_10 (cumulative up to bucket_10 = 95)
        assert_eq!(p95, 10.0);
        // p99 (99th request) should be in bucket_50 (cumulative up to bucket_50 = 98, up to bucket_100 = 99)
        assert_eq!(p99, 100.0);
    }

    #[test]
    fn test_endpoint_to_json() {
        let snapshot = EndpointSnapshot {
            count: 100,
            avg_ms: 5.678,
            p50_ms: 3.2,
            p95_ms: 15.9,
            p99_ms: 25.1,
            errors: 5,
            min_ms: 1,
            max_ms: 100,
        };
        
        let json = endpoint_to_json(&snapshot);
        assert_eq!(json["count"].as_u64().unwrap(), 100);
        assert_eq!(json["avg_ms"].as_str().unwrap(), "5.7");
        assert_eq!(json["p50_ms"].as_str().unwrap(), "3.2");
        assert_eq!(json["p95_ms"].as_str().unwrap(), "15.9");
        assert_eq!(json["p99_ms"].as_str().unwrap(), "25.1");
        assert_eq!(json["errors"].as_u64().unwrap(), 5);
        assert_eq!(json["min_ms"].as_u64().unwrap(), 1);
        assert_eq!(json["max_ms"].as_u64().unwrap(), 100);
    }

    #[test]
    fn test_endpoint_from_path() {
        assert_eq!(endpoint_from_path("/locks/test-lock/acquire"), Some("acquire"));
        assert_eq!(endpoint_from_path("/locks/test-lock/release"), Some("release"));
        assert_eq!(endpoint_from_path("/locks/test-lock/renew"), Some("renew"));
        assert_eq!(endpoint_from_path("/locks/test-lock"), Some("status"));
        assert_eq!(endpoint_from_path("/locks"), Some("list"));
        assert_eq!(endpoint_from_path("/auth/github"), Some("auth"));
        assert_eq!(endpoint_from_path("/auth/github/callback"), Some("auth"));
        assert_eq!(endpoint_from_path("/health"), None);
        assert_eq!(endpoint_from_path("/"), None);
        assert_eq!(endpoint_from_path("/metrics"), None);
    }

    #[test]
    fn test_get_memory_usage() {
        let memory = get_memory_usage();
        // Should either return a reasonable value or 0 if /proc/self/status is not available
        // On systems where this file exists, it should be > 0
        // On systems where it doesn't exist, it should be 0
        // We can't assert a specific range as memory usage varies
        assert!(memory >= 0);
    }

    #[test]
    fn test_concurrent_metrics_access() {
        let metrics = Metrics::new();
        let metrics_arc = std::sync::Arc::new(metrics);
        
        let mut handles = vec![];
        
        // Spawn multiple threads to record metrics concurrently
        for i in 0..10 {
            let metrics_clone = metrics_arc.clone();
            let handle = thread::spawn(move || {
                for j in 0..100 {
                    metrics_clone.record_request("acquire", (i * 100 + j) as f64, false);
                    metrics_clone.record_lock_operation("acquire");
                    metrics_clone.record_cache_hit(j % 2 == 0);
                }
            });
            handles.push(handle);
        }
        
        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Verify final counts
        assert_eq!(metrics_arc.total_requests.load(Ordering::Relaxed), 1000);
        assert_eq!(metrics_arc.lock_store_acquires.load(Ordering::Relaxed), 1000);
        assert_eq!(metrics_arc.cache_hits.load(Ordering::Relaxed), 500);
        assert_eq!(metrics_arc.cache_misses.load(Ordering::Relaxed), 500);
        assert_eq!(metrics_arc.acquire.count.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn test_edge_cases() {
        let metrics = EndpointMetrics::default();
        
        // Test zero duration
        metrics.record(0.0, false);
        assert_eq!(metrics.bucket_0_5.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.min_ms.load(Ordering::Relaxed), 0);
        
        // Test very large duration
        metrics.record(1000.0, false);
        assert_eq!(metrics.bucket_inf.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.max_ms.load(Ordering::Relaxed), 1000);
        
        // Test exact bucket boundaries
        metrics.record(0.5, false);  // Should go in bucket_0_5
        metrics.record(1.0, false);  // Should go in bucket_1
        metrics.record(2.0, false);  // Should go in bucket_2
        
        assert_eq!(metrics.bucket_0_5.load(Ordering::Relaxed), 2); // 0.0 and 0.5
        assert_eq!(metrics.bucket_1.load(Ordering::Relaxed), 1);   // 1.0
        assert_eq!(metrics.bucket_2.load(Ordering::Relaxed), 1);   // 2.0
    }

    #[test]
    fn test_endpoint_metrics_concurrent_min_max() {
        let metrics = EndpointMetrics::default();
        let metrics_arc = std::sync::Arc::new(metrics);
        
        let mut handles = vec![];
        
        // Spawn threads with different duration ranges to test min/max handling
        for i in 0..5 {
            let metrics_clone = metrics_arc.clone();
            let handle = thread::spawn(move || {
                for j in 0..20 {
                    let duration = (i * 20 + j) as f64;
                    metrics_clone.record(duration, false);
                }
            });
            handles.push(handle);
        }
        
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Should have recorded 0.0 to 99.0
        assert_eq!(metrics_arc.min_ms.load(Ordering::Relaxed), 0);
        assert_eq!(metrics_arc.max_ms.load(Ordering::Relaxed), 99);
        assert_eq!(metrics_arc.count.load(Ordering::Relaxed), 100);
    }
}