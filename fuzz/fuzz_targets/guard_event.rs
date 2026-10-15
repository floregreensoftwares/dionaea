#![no_main]

use std::sync::OnceLock;

use dionaea_core::{Schema, parse_schema};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

const SCHEMA: &str = "[events.probe.fields.i]\nkind = \"id\"\n\
    [events.probe.fields.t]\nkind = \"text\"\nmax = 8\n\
    [events.probe.fields.u]\nkind = \"unit\"\n\
    [events.probe.fields.e]\nkind = \"enum\"\nvalues = [\"a\", \"us-east\"]\n\
    [events.probe.fields.c]\nkind = \"count\"\nmax = 10\n\
    [events.probe.fields.d]\nkind = \"duration\"\nmax = 5000\n\
    [traits.plan]\nkind = \"enum\"\nvalues = [\"free\", \"pro\"]\n";

fn schema() -> &'static Schema {
    static SCHEMA_ONCE: OnceLock<Schema> = OnceLock::new();
    SCHEMA_ONCE.get_or_init(|| parse_schema(SCHEMA).expect("the fuzz schema is valid"))
}

// A JSON object as the properties of an event and as identity traits: no
// panic, and every property is accounted for exactly once.
fuzz_target!(|data: &[u8]| {
    let Ok(Value::Object(props)) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let schema = schema();
    if let Ok(guarded) = schema.guard_event("probe", &props) {
        assert_eq!(
            guarded.fields.len() + guarded.invalid.len() + guarded.undeclared,
            props.len()
        );
    }
    if let Ok(guarded) = schema.guard_traits(&props) {
        assert_eq!(
            guarded.fields.len() + guarded.invalid.len() + guarded.undeclared,
            props.len()
        );
    }
});
