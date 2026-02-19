#![no_main]
use libfuzzer_sys::fuzz_target;
use octostore::models::{validate_ttl, validate_metadata, AcquireLockRequest};

fuzz_target!(|data: &[u8]| {
    if let Ok(req) = serde_json::from_slice::<AcquireLockRequest>(data) {
        if let Some(ttl) = req.ttl_seconds {
            let _ = validate_ttl(ttl);
        }
        let _ = validate_metadata(&req.metadata);
    }
});
