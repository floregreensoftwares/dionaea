#![no_main]

use dionaea_core::{MAX_DIAGNOSTICS, diff, parse_schema};
use libfuzzer_sys::fuzz_target;

// Any text: no panic; an accepted file equals itself; diagnostics are bounded
// and positioned inside the file.
fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    match parse_schema(&source) {
        Ok(schema) => assert!(diff(&schema, &schema).is_empty()),
        Err(diagnostics) => {
            assert!(!diagnostics.is_empty() && diagnostics.len() <= MAX_DIAGNOSTICS + 1);
            let lines = source.lines().count().max(1) + 1;
            for d in &diagnostics {
                assert!(d.line >= 1 && d.line <= lines && d.column >= 1);
            }
        }
    }
});
