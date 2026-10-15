use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::name::Name;
use crate::schema::{FieldDef, FieldKind, MAX_PROPERTIES, Schema};

const MAX_ID_LEN: usize = 64;

/// Why an event was not accepted. Counters are keyed by these, never by the
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    InvalidEventName,
    UnknownEvent,
    TooManyProperties,
    MissingRequiredField,
    InvalidRequiredField,
}

impl DropReason {
    pub const ALL: [Self; 5] = [
        Self::InvalidEventName,
        Self::UnknownEvent,
        Self::TooManyProperties,
        Self::MissingRequiredField,
        Self::InvalidRequiredField,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidEventName => "invalid_event_name",
            Self::UnknownEvent => "unknown_event",
            Self::TooManyProperties => "too_many_properties",
            Self::MissingRequiredField => "missing_required_field",
            Self::InvalidRequiredField => "invalid_required_field",
        }
    }
}

/// A value that passed its field's rules.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// From `id`, `text` and `enum` fields.
    Text(String),
    /// From `unit` fields: within 0..1, three decimals.
    Unit(f64),
    /// From `count` and `duration` fields.
    Integer(u64),
}

/// A sanitized event or set of traits, plus how much was removed on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Guarded {
    pub fields: BTreeMap<Name, FieldValue>,
    /// Properties the schema does not declare, removed.
    pub undeclared: usize,
    /// Declared optional properties whose value broke the rules, removed. Only
    /// declared names are listed, never undeclared names or any value.
    pub invalid: Vec<Name>,
}

impl Schema {
    /// Validates one event against the schema. The result holds only declared
    /// fields with valid values; anything else is stripped, and an event that
    /// cannot stand without a required field is dropped.
    pub fn guard_event(
        &self,
        name: &str,
        properties: &Map<String, Value>,
    ) -> Result<Guarded, DropReason> {
        if !Name::is_valid(name) {
            return Err(DropReason::InvalidEventName);
        }
        let def = self.event(name).ok_or(DropReason::UnknownEvent)?;
        guard_fields(def.fields(), properties, true)
    }

    /// Validates identity traits. Every trait is optional.
    pub fn guard_traits(&self, traits: &Map<String, Value>) -> Result<Guarded, DropReason> {
        guard_fields(self.traits(), traits, false)
    }
}

fn guard_fields(
    declared: &BTreeMap<Name, FieldDef>,
    properties: &Map<String, Value>,
    enforce_required: bool,
) -> Result<Guarded, DropReason> {
    if properties.len() > MAX_PROPERTIES {
        return Err(DropReason::TooManyProperties);
    }
    let mut fields = BTreeMap::new();
    let mut present = 0;
    let mut invalid = Vec::new();
    for (name, def) in declared {
        let required = def.required && enforce_required;
        let Some(value) = properties.get(name.as_str()) else {
            if required {
                return Err(DropReason::MissingRequiredField);
            }
            continue;
        };
        present += 1;
        match sanitize(&def.kind, value) {
            Some(value) => {
                fields.insert(name.clone(), value);
            }
            None if required => return Err(DropReason::InvalidRequiredField),
            None => invalid.push(name.clone()),
        }
    }
    Ok(Guarded {
        fields,
        undeclared: properties.len() - present,
        invalid,
    })
}

/// Characters `text` replaces with a space: Unicode controls (Cc), the format
/// characters (Cf) that can reorder or hide text, and the line and paragraph
/// separators. The standard library has no category lookup, so the Cf set is
/// listed by hand from the Unicode character database.
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            u32::from(c),
            0x00AD
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x180E
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
                | 0xFFF9..=0xFFFB
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0001
                | 0xE0020..=0xE007F
        )
}

fn sanitize(kind: &FieldKind, value: &Value) -> Option<FieldValue> {
    match kind {
        FieldKind::Id => {
            let text = value.as_str()?;
            let shaped = !text.is_empty()
                && text.len() <= MAX_ID_LEN
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
            shaped.then(|| FieldValue::Text(text.to_owned()))
        }
        FieldKind::Text { max } => {
            let capped: String = value
                .as_str()?
                .chars()
                .map(|c| if is_unsafe_char(c) { ' ' } else { c })
                .skip_while(|c| c.is_whitespace())
                .take(*max)
                .collect();
            let capped = capped.trim_end();
            (!capped.is_empty()).then(|| FieldValue::Text(capped.to_owned()))
        }
        FieldKind::Unit => {
            let number = value.as_f64().filter(|n| n.is_finite())?;
            // Adding 0.0 turns a negative zero into a plain zero.
            Some(FieldValue::Unit(
                (number.clamp(0.0, 1.0) * 1000.0).round() / 1000.0 + 0.0,
            ))
        }
        FieldKind::Enum { values } => {
            let text = value.as_str()?;
            values
                .contains(text)
                .then(|| FieldValue::Text(text.to_owned()))
        }
        FieldKind::Count { max } | FieldKind::Duration { max } => {
            value.as_u64().filter(|n| n <= max).map(FieldValue::Integer)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::name::EnumValue;
    use crate::schema::EventDef;

    fn value(text: &str) -> EnumValue {
        EnumValue::new(text).unwrap()
    }

    fn name(value: &str) -> Name {
        Name::new(value).unwrap()
    }

    fn props(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// One event `probe` with a single field `f` of the given kind.
    fn probe(kind: FieldKind, required: bool) -> Schema {
        let field = FieldDef { kind, required };
        let def = EventDef::new(BTreeMap::from([(name("f"), field)])).unwrap();
        Schema::new(BTreeMap::from([(name("probe"), def)]), BTreeMap::new()).unwrap()
    }

    fn guard_one(kind: FieldKind, value: Value) -> Option<FieldValue> {
        let schema = probe(kind, true);
        schema
            .guard_event("probe", &props(json!({ "f": value })))
            .ok()
            .and_then(|g| g.fields.get(&name("f")).cloned())
    }

    fn text(value: &str) -> Option<FieldValue> {
        Some(FieldValue::Text(value.to_owned()))
    }

    #[test]
    fn id_accepts_identifier_shaped_strings() {
        for value in ["abc", "A-1_b.c:d", "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e"] {
            assert_eq!(
                guard_one(FieldKind::Id, json!(value)),
                text(value),
                "{value}"
            );
        }
    }

    #[test]
    fn id_rejects_whitespace_symbols_empty_long_and_non_strings() {
        let long = "a".repeat(MAX_ID_LEN + 1);
        for value in [
            json!(""),
            json!("a b"),
            json!("a/b"),
            json!("é"),
            json!(long),
            json!(1),
            json!(null),
        ] {
            assert_eq!(guard_one(FieldKind::Id, value.clone()), None, "{value}");
        }
        assert!(guard_one(FieldKind::Id, json!("a".repeat(MAX_ID_LEN))).is_some());
    }

    #[test]
    fn text_replaces_control_characters_and_trims() {
        let kind = FieldKind::text(50).unwrap();
        assert_eq!(guard_one(kind, json!("  a\tb\nc\u{0}d  ")), text("a b c d"));
    }

    #[test]
    fn text_replaces_format_characters_and_line_separators() {
        let kind = FieldKind::text(50).unwrap();
        let cases = [
            ("a\u{202E}b", "a b"),
            ("a\u{200B}b", "a b"),
            ("a\u{2066}b\u{2069}c", "a b c"),
            ("a\u{FEFF}b", "a b"),
            ("a\u{2028}b\u{2029}c", "a b c"),
            ("a\u{E0041}b", "a b"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                guard_one(kind.clone(), json!(input)),
                text(expected),
                "{input:?}"
            );
        }
    }

    #[test]
    fn text_keeps_ordinary_non_ascii_characters() {
        let kind = FieldKind::text(50).unwrap();
        assert_eq!(guard_one(kind, json!("café 日本 ✓")), text("café 日本 ✓"));
    }

    #[test]
    fn text_is_cut_at_the_declared_maximum() {
        let kind = FieldKind::text(3).unwrap();
        assert_eq!(guard_one(kind.clone(), json!("abcdef")), text("abc"));
        assert_eq!(guard_one(kind, json!("ééééé")), text("ééé"));
    }

    #[test]
    fn text_that_is_empty_after_cleaning_is_invalid() {
        let kind = FieldKind::text(10).unwrap();
        assert_eq!(guard_one(kind.clone(), json!("  \n ")), None);
        assert_eq!(guard_one(kind, json!(5)), None);
    }

    #[test]
    fn unit_is_clamped_and_rounded_to_three_decimals() {
        let cases = [
            (0.5, 0.5),
            (1.7, 1.0),
            (-3.0, 0.0),
            (0.12345, 0.123),
            (0.1236, 0.124),
        ];
        for (input, expected) in cases {
            assert_eq!(
                guard_one(FieldKind::Unit, json!(input)),
                Some(FieldValue::Unit(expected)),
                "{input}"
            );
        }
    }

    #[test]
    fn unit_never_yields_negative_zero() {
        let Some(FieldValue::Unit(value)) = guard_one(FieldKind::Unit, json!(-0.0)) else {
            panic!("expected a unit value");
        };
        assert!(value.is_sign_positive());
    }

    #[test]
    fn unit_rejects_non_numbers() {
        for value in [json!("0.5"), json!(null), json!([0.5])] {
            assert_eq!(guard_one(FieldKind::Unit, value), None);
        }
    }

    #[test]
    fn enum_values_may_use_hyphens_dots_and_leading_digits() {
        let kind = FieldKind::enumeration([value("us-east"), value("3d"), value("v1.2")]).unwrap();
        for declared in ["us-east", "3d", "v1.2"] {
            assert_eq!(guard_one(kind.clone(), json!(declared)), text(declared));
        }
        assert_eq!(guard_one(kind, json!("US-EAST")), None);
    }

    #[test]
    fn enum_accepts_only_declared_values() {
        let kind = FieldKind::enumeration([value("a"), value("b")]).unwrap();
        assert_eq!(guard_one(kind.clone(), json!("a")), text("a"));
        assert_eq!(guard_one(kind.clone(), json!("c")), None);
        assert_eq!(guard_one(kind.clone(), json!("A")), None);
        assert_eq!(guard_one(kind, json!(1)), None);
    }

    #[test]
    fn count_accepts_integers_up_to_the_maximum() {
        let kind = FieldKind::count(10).unwrap();
        assert_eq!(
            guard_one(kind.clone(), json!(0)),
            Some(FieldValue::Integer(0))
        );
        assert_eq!(
            guard_one(kind.clone(), json!(10)),
            Some(FieldValue::Integer(10))
        );
        assert_eq!(guard_one(kind, json!(11)), None);
    }

    #[test]
    fn count_rejects_negative_fractional_and_non_numbers() {
        let kind = FieldKind::count(10).unwrap();
        for value in [json!(-1), json!(1.5), json!(3.0), json!("3"), json!(null)] {
            assert_eq!(guard_one(kind.clone(), value.clone()), None, "{value}");
        }
    }

    #[test]
    fn duration_follows_the_same_rules_as_count() {
        let kind = FieldKind::duration(5000).unwrap();
        assert_eq!(
            guard_one(kind.clone(), json!(5000)),
            Some(FieldValue::Integer(5000))
        );
        assert_eq!(guard_one(kind.clone(), json!(5001)), None);
        assert_eq!(guard_one(kind, json!(-5)), None);
    }

    #[test]
    fn undeclared_fields_are_stripped_and_counted() {
        let schema = probe(FieldKind::Id, true);
        let guarded = schema
            .guard_event(
                "probe",
                &props(json!({ "f": "ok", "email": "a@b.c", "x": 1 })),
            )
            .unwrap();
        assert_eq!(guarded.fields.len(), 1);
        assert_eq!(guarded.undeclared, 2);
        assert!(guarded.invalid.is_empty());
    }

    #[test]
    fn invalid_optional_field_is_stripped_and_counted() {
        let schema = probe(FieldKind::Id, false);
        let guarded = schema
            .guard_event("probe", &props(json!({ "f": "not ok" })))
            .unwrap();
        assert!(guarded.fields.is_empty());
        assert_eq!(guarded.invalid, vec![name("f")]);
        assert_eq!(guarded.undeclared, 0);
    }

    #[test]
    fn absent_optional_field_is_fine() {
        let schema = probe(FieldKind::Id, false);
        let guarded = schema.guard_event("probe", &props(json!({}))).unwrap();
        assert!(guarded.fields.is_empty());
        assert_eq!((guarded.invalid.len(), guarded.undeclared), (0, 0));
    }

    #[test]
    fn missing_required_field_drops_the_event() {
        let schema = probe(FieldKind::Id, true);
        assert_eq!(
            schema.guard_event("probe", &props(json!({}))),
            Err(DropReason::MissingRequiredField)
        );
    }

    #[test]
    fn invalid_required_field_drops_the_event() {
        let schema = probe(FieldKind::Id, true);
        assert_eq!(
            schema.guard_event("probe", &props(json!({ "f": "not ok" }))),
            Err(DropReason::InvalidRequiredField)
        );
    }

    #[test]
    fn unknown_event_is_dropped() {
        let schema = probe(FieldKind::Id, true);
        assert_eq!(
            schema.guard_event("other", &props(json!({}))),
            Err(DropReason::UnknownEvent)
        );
    }

    #[test]
    fn malformed_event_name_is_dropped() {
        let schema = probe(FieldKind::Id, true);
        for value in ["", "Probe", "pro be", "probe\n", &"a".repeat(65)] {
            assert_eq!(
                schema.guard_event(value, &props(json!({}))),
                Err(DropReason::InvalidEventName),
                "{value:?}"
            );
        }
    }

    #[test]
    fn too_many_properties_drop_the_event_before_any_value_is_read() {
        let schema = probe(FieldKind::Id, true);
        let many: Map<String, Value> = (0..=MAX_PROPERTIES)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect();
        assert_eq!(
            schema.guard_event("probe", &many),
            Err(DropReason::TooManyProperties)
        );
    }

    #[test]
    fn exactly_the_property_limit_is_accepted() {
        let schema = probe(FieldKind::Id, true);
        let mut at_limit: Map<String, Value> = (1..MAX_PROPERTIES)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect();
        at_limit.insert("f".to_owned(), json!("ok"));
        assert_eq!(at_limit.len(), MAX_PROPERTIES);
        let guarded = schema.guard_event("probe", &at_limit).unwrap();
        assert_eq!(guarded.undeclared, MAX_PROPERTIES - 1);
    }

    #[test]
    fn nested_values_are_never_accepted() {
        for kind in [
            FieldKind::Id,
            FieldKind::Unit,
            FieldKind::text(5).unwrap(),
            FieldKind::count(5).unwrap(),
        ] {
            assert_eq!(guard_one(kind.clone(), json!({"a": 1})), None);
            assert_eq!(guard_one(kind, json!(["a"])), None);
        }
    }

    #[test]
    fn builtin_events_are_guarded_without_declaration() {
        let schema = Schema::default();
        let tap = schema
            .guard_event(
                "ui_tap",
                &props(json!({ "screen": "home", "element": "buy_button" })),
            )
            .unwrap();
        assert_eq!(tap.fields.len(), 2);
        assert_eq!(
            schema.guard_event("ui_tap", &props(json!({ "screen": "home" }))),
            Err(DropReason::MissingRequiredField)
        );
        assert_eq!(
            schema.guard_event("app_lifecycle", &props(json!({ "state": "sleeping" }))),
            Err(DropReason::InvalidRequiredField)
        );
        assert!(
            schema
                .guard_event("app_lifecycle", &props(json!({ "state": "background" })))
                .is_ok()
        );
    }

    #[test]
    fn reserved_health_events_are_unknown_until_their_shapes_exist() {
        assert_eq!(
            Schema::default().guard_event("app_error", &props(json!({}))),
            Err(DropReason::UnknownEvent)
        );
    }

    #[test]
    fn traits_are_filtered_like_optional_fields() {
        let traits = BTreeMap::from([
            (
                name("plan"),
                FieldDef::optional(FieldKind::enumeration([value("free"), value("pro")]).unwrap()),
            ),
            (
                name("seats"),
                FieldDef::optional(FieldKind::count(100).unwrap()),
            ),
        ]);
        let schema = Schema::new(BTreeMap::new(), traits).unwrap();
        let guarded = schema
            .guard_traits(&props(json!({ "seats": 500, "email": "a@b.c" })))
            .unwrap();
        assert!(guarded.fields.is_empty());
        assert_eq!(
            (guarded.invalid, guarded.undeclared),
            (vec![name("seats")], 1)
        );
        let guarded = schema
            .guard_traits(&props(json!({ "plan": "pro" })))
            .unwrap();
        assert_eq!(
            guarded.fields.get(&name("plan")),
            Some(&FieldValue::Text("pro".to_owned()))
        );
    }

    #[test]
    fn traits_beyond_the_property_limit_are_refused() {
        let many: Map<String, Value> = (0..=MAX_PROPERTIES)
            .map(|i| (format!("k{i}"), json!(i)))
            .collect();
        assert_eq!(
            Schema::default().guard_traits(&many),
            Err(DropReason::TooManyProperties)
        );
    }

    #[test]
    fn drop_reason_all_lists_every_variant() {
        // The match has no wildcard: a new variant stops this from compiling
        // until it is handled here and added to `ALL`.
        for reason in DropReason::ALL {
            match reason {
                DropReason::InvalidEventName
                | DropReason::UnknownEvent
                | DropReason::TooManyProperties
                | DropReason::MissingRequiredField
                | DropReason::InvalidRequiredField => {}
            }
        }
        let listed: std::collections::BTreeSet<_> = DropReason::ALL.into_iter().collect();
        assert_eq!(listed.len(), 5);
    }

    #[test]
    fn every_drop_reason_has_a_distinct_counter_name() {
        let names: std::collections::BTreeSet<_> =
            DropReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(names.len(), DropReason::ALL.len());
    }
}

#[cfg(test)]
mod unsafe_char_tests {
    use super::is_unsafe_char;

    fn flagged(code: u32) -> bool {
        is_unsafe_char(char::from_u32(code).unwrap())
    }

    #[test]
    fn range_edges_are_replaced() {
        for code in [
            0x00AD, 0x0600, 0x0605, 0x061C, 0x200B, 0x200F, 0x2028, 0x2029, 0x202A, 0x202E, 0x2060,
            0x2064, 0x2066, 0x206F, 0xFEFF, 0xFFF9, 0xFFFB, 0xE0001, 0xE0020, 0xE007F,
        ] {
            assert!(flagged(code), "U+{code:04X}");
        }
    }

    #[test]
    fn neighbours_of_the_ranges_are_kept() {
        for code in [
            0x00AC, 0x00AE, 0x05FF, 0x0606, 0x200A, 0x2010, 0x2027, 0x202F, 0x205F, 0x2070, 0xFEFE,
            0xFF00, 0xFFF8, 0xFFFC, 0xE0000, 0xE0002, 0xE001F, 0xE0080, 0x0041, 0x00E9, 0x65E5,
        ] {
            assert!(!flagged(code), "U+{code:04X}");
        }
    }
}
