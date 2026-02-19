#![no_main]
use libfuzzer_sys::fuzz_target;
use octostore::models::validate_lock_name;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = validate_lock_name(s);
    }
});
