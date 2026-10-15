//! Runs the conformance vectors in `vectors/` against this implementation.
//! Every SDK guard and decoder has to pass the same files; see
//! `vectors/README.md` for the format.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use dionaea_core::{DropReason, FieldValue, Schema, decode_batch, decode_identify, parse_schema};
use serde_json::{Map, Value, json};

fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors")
}

/// Every `.json` file in `vectors/<folder>`, by name.
fn files(folder: &str) -> Vec<(String, Value)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(vectors_dir().join(folder)).expect("the vectors folder exists") {
        let path = entry.expect("a readable entry").path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let text = fs::read_to_string(&path).expect("a readable file");
            let value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
            found.push((
                path.file_name()
                    .expect("a file name")
                    .to_string_lossy()
                    .into_owned(),
                value,
            ));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!found.is_empty(), "no vectors in {folder}");
    found
}

fn object<'a>(value: &'a Value, what: &str) -> &'a Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{what} must be an object"))
}

fn field_value(value: &FieldValue) -> Value {
    match value {
        FieldValue::Text(text) => Value::from(text.clone()),
        FieldValue::Unit(number) => Value::from(*number),
        FieldValue::Integer(number) => Value::from(*number),
    }
}

/// What the guard did, in the shape the vectors expect.
fn guard_outcome(
    result: Result<dionaea_core::Guarded, dionaea_core::Dropped>,
    seen: &mut BTreeSet<&'static str>,
) -> Value {
    match result {
        Ok(guarded) => {
            let fields: Map<String, Value> = guarded
                .fields
                .iter()
                .map(|(name, value)| (name.to_string(), field_value(value)))
                .collect();
            let invalid: Vec<String> = guarded.invalid.iter().map(ToString::to_string).collect();
            json!({ "accepted": {
                "fields": fields, "undeclared": guarded.undeclared, "invalid": invalid
            }})
        }
        Err(dropped) => {
            seen.insert(dropped.reason.as_str());
            let mut outcome = json!({ "dropped": { "reason": dropped.reason.as_str() } });
            if let Some(field) = dropped.field {
                outcome["dropped"]["field"] = json!(field.to_string());
            }
            outcome
        }
    }
}

fn run_guard_file(name: &str, file: &Value, seen: &mut BTreeSet<&'static str>) -> usize {
    let file = object(file, name);
    let source = file["schema"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: schema is a string"));
    let schema: Schema = parse_schema(source)
        .unwrap_or_else(|e| panic!("{name}: the vector schema is invalid: {e:?}"));
    let cases = file["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: cases is an array"));
    for case in cases {
        let case = object(case, "a case");
        let label = format!(
            "{name}: {}",
            case["name"].as_str().expect("a case has a name")
        );
        let result = if let Some(traits) = case.get("traits") {
            schema.guard_traits(object(traits, "traits"))
        } else {
            let event = case["event"]
                .as_str()
                .expect("a case has an event or traits");
            let props = object(case.get("props").unwrap_or(&json!({})), "props").clone();
            schema.guard_event(event, &props)
        };
        let actual = guard_outcome(result, seen);
        assert_eq!(actual, case["expect"], "{label}");
    }
    cases.len()
}

fn batch_outcome(
    result: Result<dionaea_core::Batch, dionaea_core::RequestError>,
    seen_reasons: &mut BTreeSet<&'static str>,
    seen_errors: &mut BTreeSet<&'static str>,
) -> Value {
    match result {
        Err(error) => {
            seen_errors.insert(error.as_str());
            let mut outcome = json!({ "request_error": error.as_str() });
            if let Some(field) = error.field() {
                outcome["field"] = json!(field);
            }
            outcome
        }
        Ok(batch) => {
            let events: Vec<Value> = batch
                .events
                .iter()
                .map(|e| {
                    json!({
                        "event_id": e.event_id.to_string(), "name": e.name, "ts": e.ts,
                        "anon_id": e.anon_id, "session_id": e.session_id, "screen": e.screen,
                        "props": e.props
                    })
                })
                .collect();
            let rejected: Vec<Value> = batch
                .rejected
                .iter()
                .map(|r| {
                    seen_reasons.insert(r.reason.as_str());
                    json!({ "index": r.index, "reason": r.reason.as_str() })
                })
                .collect();
            json!({ "batch": {
                "sent_at": batch.sent_at, "sdk": batch.sdk, "release": batch.release,
                "events": events, "rejected": rejected
            }})
        }
    }
}

fn identify_outcome(
    result: Result<dionaea_core::Identify, dionaea_core::RequestError>,
    seen_errors: &mut BTreeSet<&'static str>,
) -> Value {
    match result {
        Err(error) => {
            seen_errors.insert(error.as_str());
            let mut outcome = json!({ "request_error": error.as_str() });
            if let Some(field) = error.field() {
                outcome["field"] = json!(field);
            }
            outcome
        }
        Ok(identify) => json!({ "identify": {
            "sent_at": identify.sent_at, "sdk": identify.sdk, "anon_id": identify.anon_id,
            "user_id": identify.user_id, "traits": identify.traits
        }}),
    }
}

/// The body of a wire case: a string, or prefix + repeat x times + suffix.
fn case_body(case: &Map<String, Value>) -> Vec<u8> {
    if let Some(body) = case.get("body") {
        return body.as_str().expect("a body string").as_bytes().to_vec();
    }
    let parts = object(&case["body_parts"], "body_parts");
    let text = |key: &str| parts[key].as_str().expect("a string part").to_owned();
    let times = usize::try_from(parts["times"].as_u64().expect("times")).expect("fits");
    format!(
        "{}{}{}",
        text("prefix"),
        text("repeat").repeat(times),
        text("suffix")
    )
    .into_bytes()
}

fn run_wire_file(
    name: &str,
    file: &Value,
    seen_reasons: &mut BTreeSet<&'static str>,
    seen_errors: &mut BTreeSet<&'static str>,
) -> usize {
    let file = object(file, name);
    let endpoint = file["endpoint"].as_str().expect("an endpoint");
    let cases = file["cases"].as_array().expect("cases is an array");
    for case in cases {
        let case = object(case, "a case");
        let label = format!(
            "{name}: {}",
            case["name"].as_str().expect("a case has a name")
        );
        let body = case_body(case);
        let body = body.as_slice();
        let actual = match endpoint {
            "collect" => batch_outcome(decode_batch(body), seen_reasons, seen_errors),
            "identify" => identify_outcome(decode_identify(body), seen_errors),
            other => panic!("{name}: unknown endpoint {other}"),
        };
        assert_eq!(actual, case["expect"], "{label}");
    }
    cases.len()
}

#[test]
fn every_vector_passes_and_every_reason_is_produced() {
    let mut reasons = BTreeSet::new();
    let mut errors = BTreeSet::new();
    let mut cases = 0;
    for (name, file) in files("guard") {
        cases += run_guard_file(&name, &file, &mut reasons);
    }
    for (name, file) in files("wire") {
        cases += run_wire_file(&name, &file, &mut reasons, &mut errors);
    }
    assert!(cases > 100, "only {cases} vector cases");

    for reason in DropReason::ALL {
        assert!(
            reasons.contains(reason.as_str()),
            "no vector produces the drop reason {}",
            reason.as_str()
        );
    }
    for error in [
        "body_too_large",
        "not_json",
        "not_an_object",
        "invalid_field",
        "too_many_events",
    ] {
        assert!(
            errors.contains(error),
            "no vector produces the request error {error}"
        );
    }
}
