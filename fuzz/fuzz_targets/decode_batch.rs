#![no_main]

use dionaea_core::{MAX_BATCH_EVENTS, MAX_PROPERTIES, decode_batch};
use libfuzzer_sys::fuzz_target;

// Any bytes: no panic; what is accepted stays inside the documented limits.
fuzz_target!(|data: &[u8]| {
    if let Ok(batch) = decode_batch(data) {
        assert!(batch.events.len() + batch.rejected.len() <= MAX_BATCH_EVENTS);
        for event in &batch.events {
            assert!(event.props.len() <= MAX_PROPERTIES);
        }
    }
});
