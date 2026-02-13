#![no_main]

use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    // Test Authorization header parsing with arbitrary strings
    if let Ok(auth_header) = str::from_utf8(data) {
        let trimmed = auth_header.trim();
        
        // Test Bearer token extraction
        if trimmed.starts_with("Bearer ") {
            let token = &trimmed[7..]; // Skip "Bearer "
            
            // Test token validation (normally would be JWT or similar)
            if !token.is_empty() && token.len() <= 1000 {
                // Test as if it were a JWT-like token
                let parts: Vec<&str> = token.split('.').collect();
                if parts.len() == 3 {
                    // Each part should be base64-like
                    for part in &parts {
                        if !part.is_empty() {
                            // Test base64 decoding attempt
                            let _decode_attempt = base64::decode(part);
                        }
                    }
                }
            }
        }
        
        // Test other auth schemes
        if trimmed.starts_with("Basic ") {
            let credential = &trimmed[6..];
            let _decode_attempt = base64::decode(credential);
        }
        
        // Test malformed headers
        if trimmed.contains('\n') || trimmed.contains('\r') {
            // Header injection attempts
        }
    }
    
    // Test with raw bytes (headers should be ASCII)
    for &byte in data {
        if byte > 127 {
            // Non-ASCII in header - should be rejected
            break;
        }
    }
});