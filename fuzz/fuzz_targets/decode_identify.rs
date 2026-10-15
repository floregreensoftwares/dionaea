#![no_main]

use dionaea_core::{MAX_PROPERTIES, decode_identify};
use libfuzzer_sys::fuzz_target;

// Any bytes: no panic; accepted traits stay inside the limit.
fuzz_target!(|data: &[u8]| {
    if let Ok(identify) = decode_identify(data) {
        assert!(identify.traits.len() <= MAX_PROPERTIES);
    }
});
