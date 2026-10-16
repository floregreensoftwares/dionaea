//! Property tests over the core: whatever the input, the parser, the guard and
//! the decoders never panic, and what they return keeps its promises. They
//! live in this crate so the property-testing dependency is not built with
//! `dionaea-core` or its benchmarks.

use std::fmt::Write as _;

use dionaea_core::{
    Change, ChangeKind, DropReason, EventId, FieldValue, MAX_BATCH_EVENTS, MAX_DIAGNOSTICS,
    MAX_PROPERTIES, MAX_TIMESTAMP, Schema, decode_batch, decode_identify, diff, parse_schema,
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 192,
        ..ProptestConfig::default()
    }
}

// Strategies

/// Any JSON value, a few levels deep.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        (-1.0e6f64..1.0e6).prop_map(Value::from),
        ".{0,24}".prop_map(Value::String),
        // Characters the text rules care about.
        proptest::collection::vec(
            prop_oneof![
                Just('\u{202e}'),
                Just('\u{200b}'),
                Just('\u{3164}'),
                Just('\u{00a0}'),
                Just('\u{2800}'),
                Just('\u{034f}'),
                Just('\u{17b4}'),
                Just('\u{180b}'),
                Just('\u{fe0f}'),
                Just('\u{1f600}'),
                Just('a'),
                Just(' '),
                Just('\n'),
                Just('-'),
            ],
            0..12
        )
        .prop_map(|chars| Value::String(chars.into_iter().collect())),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map(".{0,6}", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

const DECLARED: [&str; 6] = ["i", "t", "u", "e", "c", "d"];

/// A property map that mixes the declared names with random ones.
fn properties() -> impl Strategy<Value = Map<String, Value>> {
    proptest::collection::vec(
        (
            prop_oneof![
                proptest::sample::select(DECLARED.to_vec()).prop_map(str::to_owned),
                "[a-z]{1,5}",
            ],
            json_value(),
        ),
        0..10,
    )
    .prop_map(|entries| entries.into_iter().collect())
}

/// A property map for the declared fields only, with plausible values, so the
/// accepting paths get exercised too.
fn plausible_properties() -> impl Strategy<Value = Map<String, Value>> {
    (
        proptest::option::of("[A-Za-z0-9_.:-]{0,70}"),
        proptest::option::of(".{0,20}"),
        proptest::option::of(-0.5f64..1.5),
        proptest::option::of(prop_oneof![Just("a"), Just("us-east"), Just("zzz")]),
        proptest::option::of(0u64..20),
        proptest::option::of(0u64..8000),
    )
        .prop_map(|(i, t, u, e, c, d)| {
            let mut map = Map::new();
            for (name, value) in [
                ("i", i.map(Value::from)),
                ("t", t.map(Value::from)),
                ("u", u.map(Value::from)),
                ("e", e.map(Value::from)),
                ("c", c.map(Value::from)),
                ("d", d.map(Value::from)),
            ] {
                if let Some(value) = value {
                    map.insert(name.to_owned(), value);
                }
            }
            map
        })
}

fn probe_schema() -> Schema {
    parse_schema(
        "[events.probe.fields.i]\nkind = \"id\"\n\
         [events.probe.fields.t]\nkind = \"text\"\nmax = 8\n\
         [events.probe.fields.u]\nkind = \"unit\"\n\
         [events.probe.fields.e]\nkind = \"enum\"\nvalues = [\"a\", \"us-east\"]\n\
         [events.probe.fields.c]\nkind = \"count\"\nmax = 10\n\
         [events.probe.fields.d]\nkind = \"duration\"\nmax = 5000\n",
    )
    .expect("the probe schema is valid")
}

fn required_schema() -> Schema {
    parse_schema(
        "[events.order.fields.a]\nkind = \"id\"\nrequired = true\n\
         [events.order.fields.b]\nkind = \"count\"\nmax = 5\nrequired = true\n\
         [events.order.fields.note]\nkind = \"text\"\nmax = 20\n",
    )
    .expect("the order schema is valid")
}

fn value_of(value: &FieldValue) -> Value {
    match value {
        FieldValue::Text(text) => Value::from(text.clone()),
        FieldValue::Unit(number) => Value::from(*number),
        FieldValue::Integer(number) => Value::from(*number),
    }
}

fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

// The guard

proptest! {
    #![proptest_config(config())]

    /// Whatever the properties, the guard returns only declared fields whose
    /// values obey their kind, accounts for every property exactly once, and
    /// gives the same answer when fed its own output.
    #[test]
    fn the_guard_keeps_its_promises(props in prop_oneof![properties(), plausible_properties()]) {
        let schema = probe_schema();
        let Ok(guarded) = schema.guard_event("probe", &props) else {
            // Only the property limit can drop this schema's events.
            prop_assert!(props.len() > MAX_PROPERTIES);
            return Ok(());
        };

        for (name, value) in &guarded.fields {
            prop_assert!(DECLARED.contains(&name.as_str()));
            match (name.as_str(), value) {
                ("i", FieldValue::Text(text)) => prop_assert!(is_id(text)),
                ("t", FieldValue::Text(text)) => {
                    prop_assert!(!text.is_empty());
                    prop_assert!(text.chars().count() <= 8);
                    prop_assert_eq!(text.trim(), text.as_str());
                    prop_assert!(!text.chars().any(char::is_control));
                    let invisible = text.chars().any(|c| {
                        matches!(
                            c,
                            '\u{202e}'
                                | '\u{200b}'
                                | '\u{3164}'
                                | '\u{00a0}'
                                | '\u{2800}'
                                | '\u{034f}'
                                | '\u{17b4}'
                                | '\u{180b}'
                        )
                    });
                    prop_assert!(!invisible);
                    // Variation selectors survive, but never as the whole value.
                    prop_assert!(!text.chars().all(|c| matches!(u32::from(c), 0xFE00..=0xFE0F)));
                }
                ("u", FieldValue::Unit(number)) => {
                    prop_assert!((0.0..=1.0).contains(number));
                    prop_assert!(number.is_sign_positive());
                    prop_assert_eq!((number * 1000.0).round() / 1000.0, *number);
                }
                ("e", FieldValue::Text(text)) => prop_assert!(text == "a" || text == "us-east"),
                ("c", FieldValue::Integer(n)) => prop_assert!(*n <= 10),
                ("d", FieldValue::Integer(n)) => prop_assert!(*n <= 5000),
                other => prop_assert!(false, "a value of the wrong shape: {other:?}"),
            }
        }
        for name in &guarded.invalid {
            prop_assert!(DECLARED.contains(&name.as_str()));
            prop_assert!(props.contains_key(name.as_str()));
            prop_assert!(!guarded.fields.contains_key(name));
        }
        prop_assert_eq!(
            guarded.fields.len() + guarded.invalid.len() + guarded.undeclared,
            props.len()
        );

        let again: Map<String, Value> = guarded
            .fields
            .iter()
            .map(|(name, value)| (name.to_string(), value_of(value)))
            .collect();
        let second = schema.guard_event("probe", &again).expect("clean input is accepted");
        prop_assert_eq!(&second.fields, &guarded.fields);
        prop_assert_eq!(second.undeclared, 0);
        prop_assert!(second.invalid.is_empty());
    }

    /// A dropped event names a declared required field, or none, and never
    /// anything the sender wrote.
    #[test]
    fn a_dropped_event_names_only_declared_required_fields(
        props in proptest::collection::vec(("[a-z]{1,4}", json_value()), 0..8)
    ) {
        let props: Map<String, Value> = props.into_iter().collect();
        let schema = required_schema();
        match schema.guard_event("order", &props) {
            Ok(guarded) => {
                prop_assert!(guarded.fields.contains_key("a"));
                prop_assert!(guarded.fields.contains_key("b"));
            }
            Err(dropped) => match dropped.reason {
                DropReason::MissingRequiredField => {
                    let field = dropped.field.expect("a missing field is named");
                    prop_assert!(field.as_str() == "a" || field.as_str() == "b");
                    prop_assert!(!props.contains_key(field.as_str()));
                }
                DropReason::InvalidRequiredField => {
                    let field = dropped.field.expect("an invalid field is named");
                    prop_assert!(field.as_str() == "a" || field.as_str() == "b");
                    prop_assert!(props.contains_key(field.as_str()));
                }
                DropReason::TooManyProperties => prop_assert!(props.len() > MAX_PROPERTIES),
                other => prop_assert!(false, "unexpected reason {other:?}"),
            },
        }
    }

    /// Any event name is answered without a panic, and only a valid, known
    /// name reaches the fields.
    #[test]
    fn any_event_name_is_answered(name in ".{0,80}") {
        let schema = probe_schema();
        let result = schema.guard_event(&name, &Map::new());
        if name == "probe" {
            prop_assert!(result.is_ok());
        } else if result.is_ok() {
            // A built-in event cannot be accepted with no properties.
            prop_assert!(false, "{name:?} was accepted");
        }
    }
}

// The decoders

fn valid_body(events: &[(u128, u64, &str)]) -> String {
    let items: Vec<String> = events
        .iter()
        .map(|(id, ts, name)| {
            format!(
                "{{\"event_id\":\"{}\",\"name\":\"{name}\",\"ts\":{ts},\"anon_id\":\"a1\",\"props\":{{\"k\":1}}}}",
                // A version 7 id with the variant bits set.
                EventId::parse(&format!(
                    "{:08x}-{:04x}-7{:03x}-8{:03x}-{:012x}",
                    (id >> 96) as u32,
                    (id >> 80) as u16,
                    (id >> 64) as u16 & 0xfff,
                    (id >> 48) as u16 & 0xfff,
                    id & 0xffff_ffff_ffff
                ))
                .expect("a version 7 id")
            )
        })
        .collect();
    format!(
        "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{}]}}",
        items.join(",")
    )
}

/// Applies a few random edits to a valid body.
fn mutated(body: String, edits: Vec<(usize, u8, u8)>) -> Vec<u8> {
    let mut bytes = body.into_bytes();
    for (position, kind, byte) in edits {
        if bytes.is_empty() {
            break;
        }
        let at = position % bytes.len();
        match kind % 4 {
            0 => bytes[at] = byte,
            1 => {
                bytes.remove(at);
            }
            2 => bytes.insert(at, byte),
            _ => bytes.truncate(at),
        }
    }
    bytes
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn arbitrary_bytes_never_panic_the_decoders(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let _ = decode_batch(&bytes);
        let _ = decode_identify(&bytes);
    }

    /// Edited valid bodies are the interesting ones: they get past the first
    /// checks. What comes out keeps its shape.
    #[test]
    fn edited_bodies_decode_to_well_formed_results(
        ids in proptest::collection::vec(any::<u128>(), 0..6),
        edits in proptest::collection::vec((any::<usize>(), any::<u8>(), any::<u8>()), 0..4),
    ) {
        let events: Vec<(u128, u64, &str)> =
            ids.iter().enumerate().map(|(i, id)| (*id, i as u64, "checkout")).collect();
        let bytes = mutated(valid_body(&events), edits);
        let _ = decode_identify(&bytes);
        if let Ok(batch) = decode_batch(&bytes) {
            prop_assert!(batch.events.len() + batch.rejected.len() <= MAX_BATCH_EVENTS);
            let mut last = None;
            for rejected in &batch.rejected {
                prop_assert!(last.is_none_or(|previous| previous < rejected.index));
                prop_assert!(rejected.index < MAX_BATCH_EVENTS);
                last = Some(rejected.index);
            }
            for event in &batch.events {
                prop_assert!(event.ts <= MAX_TIMESTAMP);
                prop_assert!(is_id(&event.anon_id));
                prop_assert!(event.session_id.as_deref().is_none_or(is_id));
                prop_assert!(event.props.len() <= MAX_PROPERTIES);
                prop_assert!(EventId::parse(&event.event_id.to_string()) == Some(event.event_id));
            }
        }
    }

    /// Valid bodies decode to exactly what was written.
    #[test]
    fn valid_batches_round_trip(
        ids in proptest::collection::vec(any::<u128>(), 0..=MAX_BATCH_EVENTS),
        base_ts in 0u64..1_000_000_000_000,
    ) {
        let events: Vec<(u128, u64, &str)> =
            ids.iter().enumerate().map(|(i, id)| (*id, base_ts + i as u64, "checkout")).collect();
        let batch = decode_batch(valid_body(&events).as_bytes()).expect("a valid batch");
        prop_assert!(batch.rejected.is_empty());
        prop_assert_eq!(batch.events.len(), events.len());
        for (decoded, (_, ts, name)) in batch.events.iter().zip(&events) {
            prop_assert_eq!(decoded.ts, *ts);
            prop_assert_eq!(decoded.name.as_str(), *name);
            prop_assert_eq!(decoded.props.get("k"), Some(&json!(1)));
        }
    }

    #[test]
    fn event_ids_round_trip_through_text(id in any::<u128>()) {
        let text = format!(
            "{:08x}-{:04x}-7{:03x}-8{:03x}-{:012x}",
            (id >> 96) as u32, (id >> 80) as u16, (id >> 64) as u16 & 0xfff,
            (id >> 48) as u16 & 0xfff, id & 0xffff_ffff_ffff
        );
        let parsed = EventId::parse(&text).expect("a version 7 id");
        prop_assert_eq!(parsed.to_string(), text.clone());
        prop_assert_eq!(EventId::parse(&text.to_uppercase()), Some(parsed));
    }
}

// The schema file

const VALID_FILE: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n\
    [events.checkout.fields.coupon]\nkind = \"text\"\nmax = 32\n\
    [events.checkout.fields.region]\nkind = \"enum\"\nvalues = [\"us-east\", \"eu-west\"]\n\
    [traits.plan]\nkind = \"enum\"\nvalues = [\"free\", \"pro\"]\n";

fn check_parse_result(source: &str) -> Result<(), TestCaseError> {
    match parse_schema(source) {
        Ok(schema) => {
            prop_assert!(diff(&schema, &schema).is_empty());
        }
        Err(diagnostics) => {
            prop_assert!(!diagnostics.is_empty());
            prop_assert!(diagnostics.len() <= MAX_DIAGNOSTICS + 1);
            let lines = source.lines().count().max(1) + 1;
            for d in &diagnostics {
                prop_assert!(d.line >= 1 && d.line <= lines, "line {} of {lines}", d.line);
                prop_assert!(d.column >= 1);
                prop_assert!(!d.message.is_empty());
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn arbitrary_text_never_panics_the_parser(source in "(?s).{0,300}") {
        check_parse_result(&source)?;
    }

    #[test]
    fn edited_schema_files_never_panic_the_parser(
        edits in proptest::collection::vec((any::<usize>(), any::<u8>(), any::<char>()), 1..5)
    ) {
        let mut chars: Vec<char> = VALID_FILE.chars().collect();
        for (position, kind, ch) in edits {
            if chars.is_empty() {
                break;
            }
            let at = position % chars.len();
            match kind % 3 {
                0 => chars[at] = ch,
                1 => { chars.remove(at); }
                _ => chars.insert(at, ch),
            }
        }
        let source: String = chars.into_iter().collect();
        check_parse_result(&source)?;
    }
}

/// A schema built from a small pool of names, so two draws overlap.
fn schema_text() -> impl Strategy<Value = String> {
    let kind = prop_oneof![
        Just("kind = \"id\"".to_owned()),
        Just("kind = \"unit\"".to_owned()),
        proptest::sample::select(vec![8u32, 16, 64])
            .prop_map(|max| format!("kind = \"text\"\nmax = {max}")),
        proptest::sample::select(vec![1u32, 10, 100])
            .prop_map(|max| format!("kind = \"count\"\nmax = {max}")),
        proptest::sample::select(vec![100u32, 1000])
            .prop_map(|max| format!("kind = \"duration\"\nmax = {max}")),
        proptest::sample::subsequence(vec!["a", "b", "c"], 1..=3).prop_map(|values| {
            let list: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
            format!("kind = \"enum\"\nvalues = [{}]", list.join(", "))
        }),
    ];
    let field = (kind, any::<bool>());
    let events = proptest::collection::btree_map(
        proptest::sample::select(vec!["e0", "e1", "e2"]),
        proptest::collection::btree_map(
            proptest::sample::select(vec!["f0", "f1", "f2"]),
            field.clone(),
            0..3,
        ),
        0..3,
    );
    let traits = proptest::collection::btree_map(
        proptest::sample::select(vec!["t0", "t1"]),
        field.prop_map(|(kind, _)| (kind, false)),
        0..2,
    );
    (events, traits).prop_map(|(events, traits)| {
        let mut text = String::new();
        for (event, fields) in &events {
            if fields.is_empty() {
                let _ = writeln!(text, "[events.{event}]");
            }
            for (name, (kind, required)) in fields {
                let _ = writeln!(text, "[events.{event}.fields.{name}]\n{kind}");
                if *required {
                    let _ = writeln!(text, "required = true");
                }
            }
        }
        for (name, (kind, _)) in &traits {
            let _ = writeln!(text, "[traits.{name}]\n{kind}");
        }
        text
    })
}

/// What a change is, ignoring its direction.
fn undirected(change: &Change) -> (String, &'static str) {
    let class = match &change.kind {
        ChangeKind::EventAdded | ChangeKind::EventRemoved => "event",
        ChangeKind::FieldAdded { .. } | ChangeKind::FieldRemoved => "field",
        ChangeKind::BecameRequired | ChangeKind::BecameOptional => "required",
        ChangeKind::KindChanged { .. } => "kind",
        ChangeKind::MaxChanged { .. } => "max",
        ChangeKind::EnumValuesRemoved(_) | ChangeKind::EnumValuesAdded(_) => "enum",
    };
    (change.path.clone(), class)
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn generated_schemas_are_valid_and_equal_to_themselves(text in schema_text()) {
        let schema = parse_schema(&text).expect("a generated schema is valid");
        prop_assert!(diff(&schema, &schema).is_empty());
    }

    /// Going from a to b and from b to a reports the same places, each as the
    /// opposite change.
    #[test]
    fn a_diff_and_its_reverse_report_the_same_places(a in schema_text(), b in schema_text()) {
        let (a, b) = (
            parse_schema(&a).expect("valid"),
            parse_schema(&b).expect("valid"),
        );
        let mut forward: Vec<_> = diff(&a, &b).iter().map(undirected).collect();
        let mut backward: Vec<_> = diff(&b, &a).iter().map(undirected).collect();
        forward.sort();
        backward.sort();
        prop_assert_eq!(forward, backward);
    }

    /// If every change forward is allowed, the reverse can only remove or
    /// narrow: an added optional field forward is a removal backward.
    #[test]
    fn an_added_optional_field_is_never_breaking(a in schema_text()) {
        let old = parse_schema(&a).expect("valid");
        let with_extra = format!("{a}[events.extra_event.fields.f]\nkind = \"id\"\n");
        let new = parse_schema(&with_extra).expect("valid");
        let changes = diff(&old, &new);
        prop_assert!(changes.iter().all(|c| c.severity() != dionaea_core::Severity::Breaking));
    }
}

// The 32-entry limit

proptest! {
    #![proptest_config(config())]

    /// More than 32 properties drop the event, whatever they hold, for events,
    /// for traits and in the decoder, which does not store the excess.
    #[test]
    fn more_than_the_limit_is_always_refused(
        count in (MAX_PROPERTIES + 1)..(MAX_PROPERTIES + 40),
        value in json_value(),
    ) {
        let props: Map<String, Value> =
            (0..count).map(|i| (format!("k{i}"), value.clone())).collect();
        let dropped = probe_schema().guard_event("probe", &props).expect_err("too many");
        prop_assert_eq!(dropped.reason, DropReason::TooManyProperties);
        prop_assert!(dropped.field.is_none());
        let dropped = probe_schema().guard_traits(&props).expect_err("too many");
        prop_assert_eq!(dropped.reason, DropReason::TooManyProperties);

        let entries: Vec<String> = (0..count).map(|i| format!("\"k{i}\":1")).collect();
        let body = format!(
            "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{{\"event_id\":\"0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e\",\"name\":\"e\",\"ts\":1,\"anon_id\":\"a\",\"props\":{{{}}}}}]}}",
            entries.join(",")
        );
        let batch = decode_batch(body.as_bytes()).expect("a valid envelope");
        prop_assert!(batch.events.is_empty());
        prop_assert_eq!(batch.rejected.len(), 1);
        prop_assert_eq!(batch.rejected[0].reason, DropReason::TooManyProperties);
    }

    /// Exactly the limit is accepted and every entry is accounted for.
    #[test]
    fn exactly_the_limit_is_accepted(value in json_value()) {
        let props: Map<String, Value> =
            (0..MAX_PROPERTIES).map(|i| (format!("k{i}"), value.clone())).collect();
        let guarded = probe_schema().guard_event("probe", &props).expect("at the limit");
        prop_assert_eq!(guarded.undeclared, MAX_PROPERTIES);
    }
}
