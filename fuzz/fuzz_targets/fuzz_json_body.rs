#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json;

fuzz_target!(|data: &[u8]| {
    // Test JSON body parsing for lock acquire endpoint
    if let Ok(json_str) = std::str::from_utf8(data) {
        // Try to parse as generic JSON first
        let _parse_result = serde_json::from_str::<serde_json::Value>(json_str);
        
        // Try to parse as AcquireLockRequest structure
        #[derive(serde::Deserialize)]
        struct FuzzAcquireLockRequest {
            ttl_seconds: Option<u64>,
            metadata: Option<String>,
        }
        
        let _typed_result = serde_json::from_str::<FuzzAcquireLockRequest>(json_str);
        
        // Test specific edge cases for lock requests
        if json_str.contains("ttl_seconds") {
            // Test TTL validation edge cases
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(ttl) = val.get("ttl_seconds") {
                    match ttl {
                        serde_json::Value::Number(n) => {
                            // Test numeric edge cases
                            let _as_u64 = n.as_u64();
                            let _as_i64 = n.as_i64();
                            let _as_f64 = n.as_f64();
                        }
                        serde_json::Value::String(s) => {
                            // Test string-to-number conversion
                            let _parse_attempt = s.parse::<u64>();
                        }
                        _ => {
                            // Invalid type for TTL
                        }
                    }
                }
            }
        }
        
        if json_str.contains("metadata") {
            // Test metadata size limits (usually max 1KB)
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(metadata) = val.get("metadata") {
                    if let Some(meta_str) = metadata.as_str() {
                        // Test metadata size constraints
                        let _size_check = meta_str.len() <= 1024;
                        
                        // Test for control characters or encoding issues
                        for ch in meta_str.chars() {
                            if ch.is_control() && ch != '\t' && ch != '\n' && ch != '\r' {
                                // Potential control character injection
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Test with raw bytes that might not be valid UTF-8
    let _raw_parse = serde_json::from_slice::<serde_json::Value>(data);
});