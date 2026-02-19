#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(auth_header) = std::str::from_utf8(data) {
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            let _ = token.is_empty();
        }
    }
});
