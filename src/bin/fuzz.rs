use clap::Parser;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Parser)]
#[command(name = "octostore-fuzz")]
#[command(about = "Chaos testing tool for OctoStore lock service")]
struct Args {
    #[arg(long, help = "Base URL of the OctoStore service")]
    url: String,
    
    #[arg(long, help = "Bearer token for authentication")]
    token: String,
    
    #[arg(long, default_value = "10", help = "Number of iterations per test")]
    iterations: u32,
    
    #[arg(long, default_value = "5", help = "Concurrent threads for race condition tests")]
    threads: u32,
}

struct FuzzStats {
    total_tests: AtomicU64,
    passed_tests: AtomicU64,
    failed_tests: AtomicU64,
    unexpected_errors: AtomicU64,
}

impl FuzzStats {
    fn new() -> Self {
        Self {
            total_tests: AtomicU64::new(0),
            passed_tests: AtomicU64::new(0),
            failed_tests: AtomicU64::new(0),
            unexpected_errors: AtomicU64::new(0),
        }
    }
    
    fn record_test(&self, success: bool, unexpected_error: bool) {
        self.total_tests.fetch_add(1, Ordering::Relaxed);
        if unexpected_error {
            self.unexpected_errors.fetch_add(1, Ordering::Relaxed);
        } else if success {
            self.passed_tests.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_tests.fetch_add(1, Ordering::Relaxed);
        }
    }
    
    fn print_summary(&self) {
        let total = self.total_tests.load(Ordering::Relaxed);
        let passed = self.passed_tests.load(Ordering::Relaxed);
        let failed = self.failed_tests.load(Ordering::Relaxed);
        let unexpected = self.unexpected_errors.load(Ordering::Relaxed);
        
        println!("\n=== FUZZ TEST SUMMARY ===");
        println!("Total tests: {}", total);
        println!("Passed: {} ({:.1}%)", passed, passed as f64 / total as f64 * 100.0);
        println!("Failed (expected): {} ({:.1}%)", failed, failed as f64 / total as f64 * 100.0);
        println!("Unexpected errors (500s): {} ({:.1}%)", unexpected, unexpected as f64 / total as f64 * 100.0);
        
        if unexpected > 0 {
            println!("❌ FUZZ TEST FAILED: Found {} unexpected 500 errors!", unexpected);
            std::process::exit(1);
        } else {
            println!("✅ FUZZ TEST PASSED: No unexpected errors found!");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let client = Client::new();
    let stats = Arc::new(FuzzStats::new());
    
    println!("🔥 Starting OctoStore Fuzz Testing");
    println!("URL: {}", args.url);
    println!("Iterations per test: {}", args.iterations);
    println!("Race condition threads: {}", args.threads);
    println!();

    // Test categories
    run_malformed_json_tests(&client, &args, &stats).await?;
    run_invalid_lock_names_tests(&client, &args, &stats).await?;
    run_invalid_token_tests(&client, &args, &stats).await?;
    run_massive_payload_tests(&client, &args, &stats).await?;
    run_invalid_ttl_tests(&client, &args, &stats).await?;
    run_race_condition_tests(&client, &args, &stats).await?;
    run_invalid_lease_tests(&client, &args, &stats).await?;
    run_header_injection_tests(&client, &args, &stats).await?;
    run_long_names_tests(&client, &args, &stats).await?;
    run_unicode_tests(&client, &args, &stats).await?;
    run_token_rotation_tests(&client, &args, &stats).await?;
    run_lifecycle_abuse_tests(&client, &args, &stats).await?;
    
    stats.print_summary();
    Ok(())
}

// Helper function to verify server health after each test
async fn check_health(client: &Client, base_url: &str) -> bool {
    match client.get(&format!("{}/health", base_url)).send().await {
        Ok(response) => response.status() == StatusCode::OK,
        Err(_) => false,
    }
}

// Helper function to record test result
async fn record_fuzz_test(
    client: &Client,
    base_url: &str,
    stats: &FuzzStats,
    test_name: &str,
    response_result: Result<reqwest::Response, reqwest::Error>,
    expected_status_codes: &[StatusCode],
) {
    let success = match response_result {
        Ok(response) => {
            let status = response.status();
            let is_expected = expected_status_codes.contains(&status);
            let is_server_error = status.as_u16() >= 500;
            
            if is_server_error {
                println!("❌ {}: Unexpected server error {}", test_name, status);
                stats.record_test(false, true);
                false
            } else if is_expected {
                stats.record_test(true, false);
                true
            } else {
                println!("⚠️  {}: Unexpected status {} (expected {:?})", test_name, status, expected_status_codes);
                stats.record_test(false, false);
                false
            }
        }
        Err(e) => {
            println!("❌ {}: Request failed: {}", test_name, e);
            stats.record_test(false, true);
            false
        }
    };
    
    // Check server health
    if !check_health(client, base_url).await {
        println!("💀 {}: Server health check failed!", test_name);
        stats.record_test(false, true);
    } else if success {
        print!(".");
    }
}

// 1. Malformed JSON bodies
async fn run_malformed_json_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing malformed JSON bodies...");
    
    let malformed_payloads = vec![
        ("", "empty body"),
        ("{", "incomplete JSON"),
        ("}", "invalid JSON"),
        ("{\"ttl\":", "missing value"),
        ("{\"ttl\": null}", "null TTL"),
        ("{\"metadata\": \"not_an_object\"}", "invalid metadata type"),
        ("{\"ttl\": \"not_a_number\"}", "string TTL"),
        ("{\"ttl\": true}", "boolean TTL"),
        ("{\"extra_field\": 123}", "missing TTL"),
        ("{\"ttl\": 60, \"metadata\": null}", "null metadata"),
    ];
    
    for (payload, desc) in malformed_payloads {
        for i in 0..args.iterations {
            let test_name = format!("malformed_json_{}_{}", desc.replace(" ", "_"), i);
            let url = format!("{}/locks/test-lock/acquire", args.url);
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .body(payload.to_string())
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::UNPROCESSABLE_ENTITY],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 2. Invalid lock names
async fn run_invalid_lock_names_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing invalid lock names...");
    
    let invalid_names = vec![
        ("", "empty name"),
        ("../../../etc/passwd", "path traversal"),
        ("'; DROP TABLE locks; --", "SQL injection"),
        ("<script>alert('xss')</script>", "XSS attempt"),
        ("lock with spaces", "spaces"),
        ("lock\nwith\nnewlines", "newlines"),
        ("lock\x00with\x00nulls", "null bytes"),
        ("🚀💥🔥", "emoji"),
        ("lock|with|pipes", "pipes"),
        ("lock&with&ampersands", "ampersands"),
    ];
    
    for (name, desc) in invalid_names {
        for i in 0..args.iterations {
            let test_name = format!("invalid_lock_name_{}_{}", desc.replace(" ", "_"), i);
            let encoded_name = urlencoding::encode(name);
            let url = format!("{}/locks/{}/acquire", args.url, encoded_name);
            
            let payload = json!({"ttl": 60});
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::NOT_FOUND, StatusCode::UNPROCESSABLE_ENTITY],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 3. Invalid/expired/forged bearer tokens
async fn run_invalid_token_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing invalid bearer tokens...");
    
    let invalid_tokens = vec![
        ("", "empty token"),
        ("invalid", "invalid format"),
        ("bearer-token-but-wrong", "wrong token"),
        ("ghp_faketoken123456789012345678901234567", "fake GitHub token"),
        ("../../../etc/passwd", "path traversal token"),
        ("'; DROP TABLE users; --", "SQL injection token"),
        ("\n\r\n\r", "newlines in token"),
        ("token\x00with\x00nulls", "null bytes in token"),
        ("🚀💥🔥", "emoji token"),
    ];
    
    for (token, desc) in invalid_tokens {
        for i in 0..args.iterations {
            let test_name = format!("invalid_token_{}_{}", desc.replace(" ", "_"), i);
            let url = format!("{}/locks/test-lock/acquire", args.url);
            let payload = json!({"ttl": 60});
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 4. Massive payloads (1MB+)
async fn run_massive_payload_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing massive payloads...");
    
    // Create a 1MB+ payload
    let large_string = "x".repeat(1024 * 1024); // 1MB string
    let massive_payloads = vec![
        json!({"ttl": 60, "metadata": {"large_field": large_string.clone()}}),
        json!({"ttl": 60, "metadata": {"many_fields": (0..1000).map(|i| (format!("field_{}", i), "value")).collect::<HashMap<String, &str>>()}}),
    ];
    
    for (i, payload) in massive_payloads.iter().enumerate() {
        for j in 0..std::cmp::min(args.iterations, 3) { // Limit to 3 iterations for performance
            let test_name = format!("massive_payload_{}_{}", i, j);
            let url = format!("{}/locks/test-lock/acquire", args.url);
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE, StatusCode::UNPROCESSABLE_ENTITY, StatusCode::OK],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 5. Invalid TTL values
async fn run_invalid_ttl_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing invalid TTL values...");
    
    let invalid_ttls = vec![
        (json!({"ttl": 0}), "zero TTL"),
        (json!({"ttl": -1}), "negative TTL"),
        (json!({"ttl": 999999}), "huge TTL"),
        (json!({"ttl": 1.5}), "float TTL"),
        (json!({"ttl": "60"}), "string TTL"),
        (json!({"ttl": null}), "null TTL"),
        (json!({"ttl": [60]}), "array TTL"),
        (json!({"ttl": {"value": 60}}), "object TTL"),
    ];
    
    for (payload, desc) in invalid_ttls {
        for i in 0..args.iterations {
            let test_name = format!("invalid_ttl_{}_{}", desc.replace(" ", "_"), i);
            let url = format!("{}/locks/test-lock/acquire", args.url);
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::UNPROCESSABLE_ENTITY, StatusCode::OK], // Some might be valid
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 6. Race conditions - acquire same lock from many threads
async fn run_race_condition_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing race conditions...");
    
    for i in 0..args.iterations {
        let lock_name = format!("race-lock-{}", i);
        let mut handles = vec![];
        
        for j in 0..args.threads {
            let client = client.clone();
            let url = args.url.clone();
            let token = args.token.clone();
            let lock_name = lock_name.clone();
            let stats = stats.clone();
            
            let handle = tokio::spawn(async move {
                let test_name = format!("race_condition_{}_thread_{}", i, j);
                let acquire_url = format!("{}/locks/{}/acquire", url, lock_name);
                let payload = json!({"ttl": 10});
                
                let response = client
                    .post(&acquire_url)
                    .header("Authorization", format!("Bearer {}", token))
                    .header("Content-Type", "application/json")
                    .json(&payload)
                    .send()
                    .await;
                    
                // For race conditions, expect either success (200) or conflict (409/423)
                record_fuzz_test(
                    &client,
                    &url,
                    &stats,
                    &test_name,
                    response,
                    &[StatusCode::OK, StatusCode::CONFLICT, StatusCode::LOCKED],
                ).await;
            });
            
            handles.push(handle);
        }
        
        // Wait for all threads to complete
        for handle in handles {
            handle.await?;
        }
        
        // Clean up the lock
        let release_url = format!("{}/locks/{}/release", args.url, lock_name);
        let _ = client
            .post(&release_url)
            .header("Authorization", format!("Bearer {}", args.token))
            .json(&json!({}))
            .send()
            .await;
    }
    
    println!();
    Ok(())
}

// 7. Invalid lease IDs
async fn run_invalid_lease_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing invalid lease IDs...");
    
    let invalid_lease_ids = vec![
        ("", "empty lease ID"),
        ("invalid-format", "invalid format"),
        ("00000000-0000-0000-0000-000000000000", "null UUID"),
        ("not-a-uuid", "not UUID format"),
        ("../../../etc/passwd", "path traversal"),
        ("'; DROP TABLE locks; --", "SQL injection"),
        ("🚀💥🔥", "emoji lease ID"),
    ];
    
    for (lease_id, desc) in invalid_lease_ids {
        for i in 0..args.iterations {
            let test_name = format!("invalid_lease_{}_{}", desc.replace(" ", "_"), i);
            let lock_name = format!("test-lock-{}", i);
            
            // Try to release with invalid lease ID
            let url = format!("{}/locks/{}/release", args.url, lock_name);
            let payload = json!({"lease_id": lease_id});
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::NOT_FOUND, StatusCode::FORBIDDEN, StatusCode::UNPROCESSABLE_ENTITY],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 8. Header injection
async fn run_header_injection_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing header injection...");
    
    let malicious_headers = vec![
        ("Bearer \r\nEvil: header", "CRLF injection"),
        ("Bearer \nX-Evil: injected", "LF injection"),
        ("Bearer token\r\n\r\n<script>alert('xss')</script>", "response splitting"),
    ];
    
    for (header_value, desc) in malicious_headers {
        for i in 0..args.iterations {
            let test_name = format!("header_injection_{}_{}", desc.replace(" ", "_"), i);
            let url = format!("{}/locks/test-lock/acquire", args.url);
            let payload = json!({"ttl": 60});
            
            let response = client
                .post(&url)
                .header("Authorization", header_value)
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::UNAUTHORIZED],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 9. Extremely long lock names
async fn run_long_names_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing extremely long lock names...");
    
    let long_names = vec![
        ("x".repeat(129), "129 chars"),
        ("x".repeat(256), "256 chars"),
        ("x".repeat(1024), "1KB name"),
        ("x".repeat(65536), "64KB name"),
    ];
    
    for (name, desc) in long_names {
        for i in 0..std::cmp::min(args.iterations, 3) { // Limit iterations for performance
            let test_name = format!("long_name_{}_{}", desc.replace(" ", "_"), i);
            let encoded_name = urlencoding::encode(&name);
            let url = format!("{}/locks/{}/acquire", args.url, encoded_name);
            let payload = json!({"ttl": 60});
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::URI_TOO_LONG, StatusCode::UNPROCESSABLE_ENTITY, StatusCode::OK],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 10. Unicode/emoji in lock names
async fn run_unicode_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing unicode/emoji lock names...");
    
    let unicode_names = vec![
        ("锁🔒", "Chinese + lock emoji"),
        ("🚀💥🔥", "emoji only"),
        ("тест", "Cyrillic"),
        ("اختبار", "Arabic"),
        ("テスト", "Japanese"),
        ("🏳️‍🌈", "complex emoji"),
        ("\\u0000", "escaped null"),
        ("Iñtërnâtiônàlizætiøn", "extended Latin"),
    ];
    
    for (name, desc) in unicode_names {
        for i in 0..args.iterations {
            let test_name = format!("unicode_name_{}_{}", desc.replace(" ", "_"), i);
            let encoded_name = urlencoding::encode(name);
            let url = format!("{}/locks/{}/acquire", args.url, encoded_name);
            let payload = json!({"ttl": 60});
            
            let response = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", args.token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;
                
            record_fuzz_test(
                client,
                &args.url,
                stats,
                &test_name,
                response,
                &[StatusCode::BAD_REQUEST, StatusCode::UNPROCESSABLE_ENTITY, StatusCode::OK],
            ).await;
        }
    }
    
    println!();
    Ok(())
}

// 11. Rapid token rotation
async fn run_token_rotation_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing rapid token rotation...");
    
    for i in 0..std::cmp::min(args.iterations, 5) { // Limited iterations
        let test_name = format!("token_rotation_{}", i);
        
        // Start a long-running request
        let client_clone = client.clone();
        let url = format!("{}/locks/rotation-test-{}/acquire", args.url, i);
        let token = args.token.clone();
        
        let handle = tokio::spawn(async move {
            let payload = json!({"ttl": 5});
            client_clone
                .post(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
        });
        
        // Simulate token rotation (try with different token)
        sleep(Duration::from_millis(100)).await;
        let different_token = format!("{}_rotated", args.token);
        let url2 = format!("{}/locks/rotation-test-{}-2/acquire", args.url, i);
        let payload2 = json!({"ttl": 5});
        
        let response2 = client
            .post(&url2)
            .header("Authorization", format!("Bearer {}", different_token))
            .header("Content-Type", "application/json")
            .json(&payload2)
            .send()
            .await;
        
        // Wait for the first request to complete
        let response1 = handle.await.unwrap();
        
        // Both should handle gracefully (either succeed or fail with proper error codes)
        record_fuzz_test(
            client,
            &args.url,
            stats,
            &format!("{}_req1", test_name),
            response1,
            &[StatusCode::OK, StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN],
        ).await;
        
        record_fuzz_test(
            client,
            &args.url,
            stats,
            &format!("{}_req2", test_name),
            response2,
            &[StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN],
        ).await;
    }
    
    println!();
    Ok(())
}

// 12-14. Lifecycle abuse tests
async fn run_lifecycle_abuse_tests(
    client: &Client,
    args: &Args,
    stats: &Arc<FuzzStats>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("🧪 Testing lifecycle abuse...");
    
    for i in 0..args.iterations {
        let lock_name = format!("lifecycle-test-{}", i);
        
        // 12. Release without acquire
        let test_name = format!("release_without_acquire_{}", i);
        let url = format!("{}/locks/{}/release", args.url, lock_name);
        let payload = json!({});
        
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", args.token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await;
            
        record_fuzz_test(
            client,
            &args.url,
            stats,
            &test_name,
            response,
            &[StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST, StatusCode::FORBIDDEN],
        ).await;
        
        // Try to acquire the lock first for double release test
        let acquire_url = format!("{}/locks/{}/acquire", args.url, lock_name);
        let acquire_payload = json!({"ttl": 60});
        
        let acquire_response = client
            .post(&acquire_url)
            .header("Authorization", format!("Bearer {}", args.token))
            .header("Content-Type", "application/json")
            .json(&acquire_payload)
            .send()
            .await;
            
        if let Ok(response) = acquire_response {
            if response.status() == StatusCode::OK {
                if let Ok(body) = response.json::<Value>().await {
                    if let Some(lease_id) = body.get("lease_id") {
                        // 13. Double release
                        let release_payload = json!({"lease_id": lease_id});
                        
                        // First release - should succeed
                        let response1 = client
                            .post(&url)
                            .header("Authorization", format!("Bearer {}", args.token))
                            .header("Content-Type", "application/json")
                            .json(&release_payload)
                            .send()
                            .await;
                            
                        // Second release - should fail
                        let response2 = client
                            .post(&url)
                            .header("Authorization", format!("Bearer {}", args.token))
                            .header("Content-Type", "application/json")
                            .json(&release_payload)
                            .send()
                            .await;
                        
                        record_fuzz_test(
                            client,
                            &args.url,
                            stats,
                            &format!("double_release_first_{}", i),
                            response1,
                            &[StatusCode::OK, StatusCode::NO_CONTENT],
                        ).await;
                        
                        record_fuzz_test(
                            client,
                            &args.url,
                            stats,
                            &format!("double_release_second_{}", i),
                            response2,
                            &[StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST, StatusCode::FORBIDDEN],
                        ).await;
                    }
                }
            }
        }
        
        // 14. Renew expired lock (we need to wait or set short TTL)
        let expired_lock_name = format!("expired-lock-{}", i);
        let acquire_url = format!("{}/locks/{}/acquire", args.url, expired_lock_name);
        let short_ttl_payload = json!({"ttl": 1}); // 1 second TTL
        
        let acquire_response = client
            .post(&acquire_url)
            .header("Authorization", format!("Bearer {}", args.token))
            .header("Content-Type", "application/json")
            .json(&short_ttl_payload)
            .send()
            .await;
            
        if let Ok(response) = acquire_response {
            if response.status() == StatusCode::OK {
                if let Ok(body) = response.json::<Value>().await {
                    if let Some(lease_id) = body.get("lease_id") {
                        // Wait for lock to expire
                        sleep(Duration::from_secs(2)).await;
                        
                        // Try to renew expired lock
                        let renew_url = format!("{}/locks/{}/renew", args.url, expired_lock_name);
                        let renew_payload = json!({"lease_id": lease_id, "ttl": 60});
                        
                        let response = client
                            .post(&renew_url)
                            .header("Authorization", format!("Bearer {}", args.token))
                            .header("Content-Type", "application/json")
                            .json(&renew_payload)
                            .send()
                            .await;
                            
                        record_fuzz_test(
                            client,
                            &args.url,
                            stats,
                            &format!("renew_expired_{}", i),
                            response,
                            &[StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST, StatusCode::FORBIDDEN, StatusCode::GONE],
                        ).await;
                    }
                }
            }
        }
    }
    
    println!();
    Ok(())
}