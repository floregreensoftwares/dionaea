//! Runs the health conformance vectors in `vectors/health/` against this
//! implementation. Each file is `{ "description": ..., "cases": [ { "name",
//! "input", "output" } ] }`; the rule is the function from input to output.

use std::fs;
use std::path::Path;

use dionaea_health::{normalize_frames, scrub_message, template_route};
use serde_json::Value;

fn cases(file: &str) -> Vec<(String, String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vectors/health")
        .join(file);
    let text = fs::read_to_string(&path).expect("a readable vector file");
    let value: Value = serde_json::from_str(&text).expect("valid JSON");
    let list = value["cases"].as_array().expect("cases");
    assert!(!list.is_empty());
    list.iter()
        .map(|case| {
            let field = |name: &str| case[name].as_str().expect(name).to_owned();
            (field("name"), field("input"), field("output"))
        })
        .collect()
}

fn run(file: &str, rule: impl Fn(&str) -> String) {
    for (name, input, output) in cases(file) {
        assert_eq!(rule(&input), output, "{file}: {name}");
        // Applying a rule to its own output changes nothing.
        assert_eq!(rule(&output), output, "{file}: {name} (second pass)");
    }
}

#[test]
fn scrubbing_vectors() {
    run("scrub.json", scrub_message);
}

#[test]
fn route_vectors() {
    run("route.json", template_route);
}

#[test]
fn frame_vectors() {
    run("frames.json", normalize_frames);
}
