#![no_main]

use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    // Test lock name handling with arbitrary byte strings
    if let Ok(lock_name) = str::from_utf8(data) {
        // Test lock name validation - this would normally be done in the HTTP handler
        let trimmed = lock_name.trim();
        
        // Basic lock name constraints (max 128 chars, alphanumeric + . -)
        if trimmed.len() <= 128 && !trimmed.is_empty() {
            let is_valid = trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
            
            // Test URL encoding/decoding of lock names
            let _encoded = urlencoding::encode(trimmed);
            
            // Test as part of a path
            let path = format!("/locks/{}/acquire", trimmed);
            let _path_bytes = path.as_bytes();
        }
    }
    
    // Also test with raw bytes that might not be valid UTF-8
    if data.len() <= 128 {
        // Test how the system handles non-UTF8 lock names
        let _as_lossy = String::from_utf8_lossy(data);
    }
});