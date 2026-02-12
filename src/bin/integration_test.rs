use clap::{Arg, Command};
use reqwest::Client;
use serde_json::{json, Value};
use std::{process, time::Instant};
use uuid::Uuid;

#[derive(Debug)]
struct TestResult {
    name: String,
    success: bool,
    duration_ms: u128,
    error: Option<String>,
}

struct TestRunner {
    client: Client,
    base_url: String,
    token: String,
    verbose: bool,
    results: Vec<TestResult>,
    cleanup_locks: Vec<String>,
}

impl TestRunner {
    fn new(base_url: String, token: String, verbose: bool) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            base_url,
            token,
            verbose,
            results: Vec::new(),
            cleanup_locks: Vec::new(),
        }
    }

    async fn make_request_json(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        let url = format!("{}{}", self.base_url, path);
        
        if self.verbose {
            println!("  → {} {}", method, url);
            if let Some(ref body) = body {
                println!("    Body: {}", serde_json::to_string_pretty(body).unwrap());
            }
        }

        let mut request = match method {
            "GET" => self.client.get(&url),
            "POST" => {
                let mut req = self.client.post(&url);
                if let Some(body) = body {
                    req = req.json(&body);
                }
                req
            }
            _ => return Err(format!("Unsupported method: {}", method)),
        };

        request = request.header("Authorization", format!("Bearer {}", self.token));

        let response = request.send().await
            .map_err(|e| format!("Request failed: {}", e))?;

        let status = response.status().as_u16();
        let text = response.text().await
            .map_err(|e| format!("Failed to read response: {}", e))?;
        
        if self.verbose {
            println!("    Status: {}", status);
            if let Ok(json) = serde_json::from_str::<Value>(&text) {
                println!("    Response: {}", serde_json::to_string_pretty(&json).unwrap());
            } else {
                println!("    Response: {}", text);
            }
        }

        if text.is_empty() {
            return Ok((status, json!("OK")));
        }

        let json = serde_json::from_str(&text)
            .map_err(|e| format!("Failed to parse JSON response: {} - Response: {}", e, text))?;

        Ok((status, json))
    }

    fn add_test_result(&mut self, name: String, success: bool, duration_ms: u128, error: Option<String>) {
        let status = if success { "\x1b[32m✅" } else { "\x1b[31m❌" };
        println!(" {} {:.<45} \x1b[90m({:>3}ms)\x1b[0m", status, format!("{} ", name), duration_ms);
        if let Some(ref err) = error {
            println!("    \x1b[31m{}\x1b[0m", err);
        }

        self.results.push(TestResult {
            name,
            success,
            duration_ms,
            error,
        });
    }

    fn add_cleanup_lock(&mut self, lock_name: String) {
        self.cleanup_locks.push(lock_name);
    }

    async fn cleanup(&self) {
        if !self.cleanup_locks.is_empty() {
            println!("\n🧹 Cleaning up locks...");
            for lock_name in &self.cleanup_locks {
                // Try to release the lock - we don't care if it fails
                let _ = self.make_request_json("POST", &format!("/locks/{}/release", lock_name), 
                    Some(json!({"lease_id": "00000000-0000-0000-0000-000000000000"}))).await;
            }
        }
    }

    fn generate_lock_name(&self) -> String {
        format!("integration-test-{}", Uuid::new_v4().to_string().split('-').next().unwrap())
    }

    fn print_summary(&self) {
        let total = self.results.len();
        let passed = self.results.iter().filter(|r| r.success).count();
        let failed = total - passed;

        println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        if failed == 0 { println!("\x1b[32mResults: {}/{} passed\x1b[0m", passed, total); } else { println!("\x1b[31mResults: {}/{} passed, {} failed\x1b[0m", passed, total, failed); }

        if failed > 0 {
            process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    let matches = Command::new("octostore-test")
        .about("OctoStore Integration Tests")
        .arg(Arg::new("url")
            .long("url")
            .value_name("URL")
            .help("Base URL to test against")
            .default_value("https://api.octostore.io"))
        .arg(Arg::new("token")
            .long("token")
            .value_name("TOKEN")
            .help("Bearer token for authentication")
            .required(true))
        .arg(Arg::new("verbose")
            .long("verbose")
            .help("Print detailed request/response info")
            .action(clap::ArgAction::SetTrue))
        .get_matches();

    let base_url = matches.get_one::<String>("url").unwrap().trim_end_matches('/');
    let token = matches.get_one::<String>("token").unwrap();
    let verbose = matches.get_flag("verbose");

    println!("\x1b[1m🐙 OctoStore Integration Tests\x1b[0m");
    println!("\x1b[90m   Target: {}\x1b[0m\n", base_url);

    let mut runner = TestRunner::new(base_url.to_string(), token.to_string(), verbose);

    // Generate test lock names
    let lock_name1 = runner.generate_lock_name();
    let lock_name2 = runner.generate_lock_name();
    runner.add_cleanup_lock(lock_name1.clone());
    runner.add_cleanup_lock(lock_name2.clone());

    let mut lease_id1: Option<String> = None;
    let mut fencing_token1: Option<u64> = None;
    let mut lease_id2: Option<String> = None;

    // Test 1: Health Check
    let start = Instant::now();
    match runner.make_request_json("GET", "/health", None).await {
        Ok((status, _)) => {
            if status == 200 {
                runner.add_test_result("Health Check".to_string(), true, start.elapsed().as_millis(), None);
            } else {
                runner.add_test_result("Health Check".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Health Check".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 2: Auth Validation
    let start = Instant::now();
    let bad_token_runner = TestRunner::new(runner.base_url.clone(), "bad-token".to_string(), runner.verbose);
    match bad_token_runner.make_request_json("GET", "/locks", None).await {
        Ok((status, _)) => {
            if status == 401 {
                runner.add_test_result("Auth Validation".to_string(), true, start.elapsed().as_millis(), None);
            } else {
                runner.add_test_result("Auth Validation".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 401 for bad token, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Auth Validation".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 3: Acquire Lock
    let start = Instant::now();
    match runner.make_request_json("POST", &format!("/locks/{}/acquire", lock_name1), 
            Some(json!({"ttl_seconds": 60}))).await {
        Ok((status, response)) => {
            if status == 200 {
                if let Some(status_field) = response.get("status").and_then(|s| s.as_str()) {
                    if status_field == "acquired" {
                        lease_id1 = response.get("lease_id").and_then(|s| s.as_str()).map(|s| s.to_string());
                        fencing_token1 = response.get("fencing_token").and_then(|s| s.as_u64());
                        
                        if lease_id1.is_some() && fencing_token1.is_some() {
                            runner.add_test_result("Acquire Lock".to_string(), true, start.elapsed().as_millis(), None);
                        } else {
                            runner.add_test_result("Acquire Lock".to_string(), false, start.elapsed().as_millis(), 
                                Some("Missing lease_id or fencing_token in response".to_string()));
                        }
                    } else {
                        runner.add_test_result("Acquire Lock".to_string(), false, start.elapsed().as_millis(), 
                            Some(format!("Expected status 'acquired', got '{}'", status_field)));
                    }
                } else {
                    runner.add_test_result("Acquire Lock".to_string(), false, start.elapsed().as_millis(), 
                        Some("Missing status field".to_string()));
                }
            } else {
                runner.add_test_result("Acquire Lock".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Acquire Lock".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 4: Lock Status (held)
    let start = Instant::now();
    match runner.make_request_json("GET", &format!("/locks/{}", lock_name1), None).await {
        Ok((status, response)) => {
            if status == 200 {
                if let Some(status_field) = response.get("status").and_then(|s| s.as_str()) {
                    if status_field == "held" {
                        runner.add_test_result("Lock Status (held)".to_string(), true, start.elapsed().as_millis(), None);
                    } else {
                        runner.add_test_result("Lock Status (held)".to_string(), false, start.elapsed().as_millis(), 
                            Some(format!("Expected status 'held', got '{}'", status_field)));
                    }
                } else {
                    runner.add_test_result("Lock Status (held)".to_string(), false, start.elapsed().as_millis(), 
                        Some("Missing status field".to_string()));
                }
            } else {
                runner.add_test_result("Lock Status (held)".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Lock Status (held)".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 5: Double Acquire (idempotent)
    let start = Instant::now();
    match runner.make_request_json("POST", &format!("/locks/{}/acquire", lock_name1), 
            Some(json!({"ttl_seconds": 60}))).await {
        Ok((status, response)) => {
            if status == 200 {
                if let Some(returned_lease_id) = response.get("lease_id").and_then(|s| s.as_str()) {
                    if Some(returned_lease_id) == lease_id1.as_deref() {
                        runner.add_test_result("Double Acquire (idempotent)".to_string(), true, start.elapsed().as_millis(), None);
                    } else {
                        runner.add_test_result("Double Acquire (idempotent)".to_string(), false, start.elapsed().as_millis(), 
                            Some("Expected same lease_id, got different one".to_string()));
                    }
                } else {
                    runner.add_test_result("Double Acquire (idempotent)".to_string(), false, start.elapsed().as_millis(), 
                        Some("Missing lease_id in response".to_string()));
                }
            } else {
                runner.add_test_result("Double Acquire (idempotent)".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Double Acquire (idempotent)".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 6: Contention Test
    let start = Instant::now();
    match runner.make_request_json("POST", &format!("/locks/{}/acquire", lock_name2), 
            Some(json!({"ttl_seconds": 60}))).await {
        Ok((status, response)) => {
            if status == 200 {
                if let Some(status_field) = response.get("status").and_then(|s| s.as_str()) {
                    if status_field == "acquired" {
                        lease_id2 = response.get("lease_id").and_then(|s| s.as_str()).map(|s| s.to_string());
                        runner.add_test_result("Contention Test".to_string(), true, start.elapsed().as_millis(), None);
                    } else {
                        runner.add_test_result("Contention Test".to_string(), false, start.elapsed().as_millis(), 
                            Some(format!("Expected status 'acquired', got '{}'", status_field)));
                    }
                } else {
                    runner.add_test_result("Contention Test".to_string(), false, start.elapsed().as_millis(), 
                        Some("Missing status field".to_string()));
                }
            } else {
                runner.add_test_result("Contention Test".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Contention Test".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 7: List User Locks
    let start = Instant::now();
    match runner.make_request_json("GET", "/locks", None).await {
        Ok((status, response)) => {
            if status == 200 {
                if let Some(locks) = response.get("locks").and_then(|l| l.as_array()) {
                    let has_lock1 = locks.iter().any(|lock| 
                        lock.get("name").and_then(|n| n.as_str()) == Some(&lock_name1)
                    );
                    let has_lock2 = locks.iter().any(|lock| 
                        lock.get("name").and_then(|n| n.as_str()) == Some(&lock_name2)
                    );

                    if has_lock1 && has_lock2 {
                        runner.add_test_result("List User Locks".to_string(), true, start.elapsed().as_millis(), None);
                    } else {
                        runner.add_test_result("List User Locks".to_string(), false, start.elapsed().as_millis(), 
                            Some("Both test locks should be present in user locks list".to_string()));
                    }
                } else {
                    runner.add_test_result("List User Locks".to_string(), false, start.elapsed().as_millis(), 
                        Some("Missing or invalid locks array".to_string()));
                }
            } else {
                runner.add_test_result("List User Locks".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("List User Locks".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 8: Renew Lock
    let start = Instant::now();
    if let Some(ref lease_id) = lease_id1 {
        match runner.make_request_json("POST", &format!("/locks/{}/renew", lock_name1), 
                Some(json!({"lease_id": lease_id, "ttl_seconds": 90}))).await {
            Ok((status, response)) => {
                if status == 200 {
                    if let Some(new_expires_at) = response.get("expires_at") {
                        if !new_expires_at.is_null() {
                            runner.add_test_result("Renew Lock".to_string(), true, start.elapsed().as_millis(), None);
                        } else {
                            runner.add_test_result("Renew Lock".to_string(), false, start.elapsed().as_millis(), 
                                Some("Expected non-null expires_at".to_string()));
                        }
                    } else {
                        runner.add_test_result("Renew Lock".to_string(), false, start.elapsed().as_millis(), 
                            Some("Missing expires_at in response".to_string()));
                    }
                } else {
                    runner.add_test_result("Renew Lock".to_string(), false, start.elapsed().as_millis(), 
                        Some(format!("Expected status 200, got {}", status)));
                }
            }
            Err(e) => {
                runner.add_test_result("Renew Lock".to_string(), false, start.elapsed().as_millis(), Some(e));
            }
        }
    } else {
        runner.add_test_result("Renew Lock".to_string(), false, start.elapsed().as_millis(), 
            Some("No lease_id available from previous test".to_string()));
    }

    // Test 9: Release Lock
    let start = Instant::now();
    if let Some(ref lease_id) = lease_id1 {
        match runner.make_request_json("POST", &format!("/locks/{}/release", lock_name1), 
                Some(json!({"lease_id": lease_id}))).await {
            Ok((status, _)) => {
                if status == 200 {
                    runner.add_test_result("Release Lock".to_string(), true, start.elapsed().as_millis(), None);
                } else {
                    runner.add_test_result("Release Lock".to_string(), false, start.elapsed().as_millis(), 
                        Some(format!("Expected status 200, got {}", status)));
                }
            }
            Err(e) => {
                runner.add_test_result("Release Lock".to_string(), false, start.elapsed().as_millis(), Some(e));
            }
        }
    } else {
        runner.add_test_result("Release Lock".to_string(), false, start.elapsed().as_millis(), 
            Some("No lease_id available from previous test".to_string()));
    }

    // Test 10: Lock Status (free)
    let start = Instant::now();
    match runner.make_request_json("GET", &format!("/locks/{}", lock_name1), None).await {
        Ok((status, response)) => {
            if status == 200 {
                if let Some(status_field) = response.get("status").and_then(|s| s.as_str()) {
                    if status_field == "free" {
                        runner.add_test_result("Lock Status (free)".to_string(), true, start.elapsed().as_millis(), None);
                    } else {
                        runner.add_test_result("Lock Status (free)".to_string(), false, start.elapsed().as_millis(), 
                            Some(format!("Expected status 'free', got '{}'", status_field)));
                    }
                } else {
                    runner.add_test_result("Lock Status (free)".to_string(), false, start.elapsed().as_millis(), 
                        Some("Missing status field".to_string()));
                }
            } else {
                runner.add_test_result("Lock Status (free)".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 200, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Lock Status (free)".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 11: Release Second Lock
    let start = Instant::now();
    if let Some(ref lease_id) = lease_id2 {
        match runner.make_request_json("POST", &format!("/locks/{}/release", lock_name2), 
                Some(json!({"lease_id": lease_id}))).await {
            Ok((status, _)) => {
                if status == 200 {
                    runner.add_test_result("Release Second Lock".to_string(), true, start.elapsed().as_millis(), None);
                } else {
                    runner.add_test_result("Release Second Lock".to_string(), false, start.elapsed().as_millis(), 
                        Some(format!("Expected status 200, got {}", status)));
                }
            }
            Err(e) => {
                runner.add_test_result("Release Second Lock".to_string(), false, start.elapsed().as_millis(), Some(e));
            }
        }
    } else {
        runner.add_test_result("Release Second Lock".to_string(), false, start.elapsed().as_millis(), 
            Some("No lease_id available from previous test".to_string()));
    }

    // Test 12: Invalid Lock Name
    let start = Instant::now();
    let invalid_name = "invalid!@#$%name";
    match runner.make_request_json("POST", &format!("/locks/{}/acquire", invalid_name), 
            Some(json!({"ttl_seconds": 60}))).await {
        Ok((status, _)) => {
            if status == 400 {
                runner.add_test_result("Invalid Lock Name".to_string(), true, start.elapsed().as_millis(), None);
            } else {
                runner.add_test_result("Invalid Lock Name".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 400 for invalid lock name, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Invalid Lock Name".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 13: Invalid TTL
    let start = Instant::now();
    let test_lock = runner.generate_lock_name();
    match runner.make_request_json("POST", &format!("/locks/{}/acquire", test_lock), 
            Some(json!({"ttl_seconds": 0}))).await {
        Ok((status, _)) => {
            if status == 400 {
                runner.add_test_result("Invalid TTL".to_string(), true, start.elapsed().as_millis(), None);
            } else {
                runner.add_test_result("Invalid TTL".to_string(), false, start.elapsed().as_millis(), 
                    Some(format!("Expected status 400 for invalid TTL, got {}", status)));
            }
        }
        Err(e) => {
            runner.add_test_result("Invalid TTL".to_string(), false, start.elapsed().as_millis(), Some(e));
        }
    }

    // Test 14: Fencing Token Monotonicity
    let start = Instant::now();
    let test_lock = runner.generate_lock_name();
    
    let mut success = false;
    let mut error_msg = None;

    match runner.make_request_json("POST", &format!("/locks/{}/acquire", test_lock), 
            Some(json!({"ttl_seconds": 60}))).await {
        Ok((status1, response1)) => {
            if status1 == 200 {
                if let (Some(lease_id), Some(first_token)) = (
                    response1.get("lease_id").and_then(|s| s.as_str()),
                    response1.get("fencing_token").and_then(|s| s.as_u64())
                ) {
                    // Release
                    match runner.make_request_json("POST", &format!("/locks/{}/release", test_lock), 
                            Some(json!({"lease_id": lease_id}))).await {
                        Ok((status2, _)) => {
                            if status2 == 200 {
                                // Second acquire
                                match runner.make_request_json("POST", &format!("/locks/{}/acquire", test_lock), 
                                        Some(json!({"ttl_seconds": 60}))).await {
                                    Ok((status3, response3)) => {
                                        if status3 == 200 {
                                            if let Some(second_token) = response3.get("fencing_token").and_then(|s| s.as_u64()) {
                                                if second_token > first_token {
                                                    success = true;
                                                    // Clean up
                                                    if let Some(final_lease_id) = response3.get("lease_id").and_then(|s| s.as_str()) {
                                                        let _ = runner.make_request_json("POST", &format!("/locks/{}/release", test_lock), 
                                                            Some(json!({"lease_id": final_lease_id}))).await;
                                                    }
                                                } else {
                                                    error_msg = Some(format!("Expected fencing_token > {}, got {}", first_token, second_token));
                                                }
                                            } else {
                                                error_msg = Some("Missing fencing_token in second response".to_string());
                                            }
                                        } else {
                                            error_msg = Some(format!("Second acquire failed with status {}", status3));
                                        }
                                    }
                                    Err(e) => {
                                        error_msg = Some(format!("Second acquire failed: {}", e));
                                    }
                                }
                            } else {
                                error_msg = Some(format!("Release failed with status {}", status2));
                            }
                        }
                        Err(e) => {
                            error_msg = Some(format!("Release failed: {}", e));
                        }
                    }
                } else {
                    error_msg = Some("Missing lease_id or fencing_token".to_string());
                }
            } else {
                error_msg = Some(format!("First acquire failed with status {}", status1));
            }
        }
        Err(e) => {
            error_msg = Some(format!("First acquire failed: {}", e));
        }
    }

    runner.add_test_result("Fencing Token Monotonicity".to_string(), success, start.elapsed().as_millis(), error_msg);

    // Cleanup
    runner.cleanup().await;

    // Print summary
    runner.print_summary();
}