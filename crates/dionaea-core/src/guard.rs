use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::name::Name;
use crate::schema::{FieldDef, FieldKind, MAX_PROPERTIES, Schema};

pub(crate) const MAX_ID_LEN: usize = 64;

/// Why an event was not accepted. Counters are keyed by these, never by the
/// payload. The first group comes from reading the request, the second from
/// the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    MalformedEvent,
    InvalidEventId,
    InvalidTimestamp,
    InvalidAnonId,
    InvalidSessionId,
    InvalidScreen,
    InvalidProperties,
    InvalidEventName,
    UnknownEvent,
    TooManyProperties,
    MissingRequiredField,
    InvalidRequiredField,
}

impl DropReason {
    pub const ALL: [Self; 12] = [
        Self::MalformedEvent,
        Self::InvalidEventId,
        Self::InvalidTimestamp,
        Self::InvalidAnonId,
        Self::InvalidSessionId,
        Self::InvalidScreen,
        Self::InvalidProperties,
        Self::InvalidEventName,
        Self::UnknownEvent,
        Self::TooManyProperties,
        Self::MissingRequiredField,
        Self::InvalidRequiredField,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MalformedEvent => "malformed_event",
            Self::InvalidEventId => "invalid_event_id",
            Self::InvalidTimestamp => "invalid_timestamp",
            Self::InvalidAnonId => "invalid_anon_id",
            Self::InvalidSessionId => "invalid_session_id",
            Self::InvalidScreen => "invalid_screen",
            Self::InvalidProperties => "invalid_properties",
            Self::InvalidEventName => "invalid_event_name",
            Self::UnknownEvent => "unknown_event",
            Self::TooManyProperties => "too_many_properties",
            Self::MissingRequiredField => "missing_required_field",
            Self::InvalidRequiredField => "invalid_required_field",
        }
    }
}

/// An event the guard did not accept: why, and for a required field that
/// failed, which declared field it was. The field is a declared name, never
/// a value or an undeclared name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub reason: DropReason,
    pub field: Option<Name>,
}

impl From<DropReason> for Dropped {
    fn from(reason: DropReason) -> Self {
        Self {
            reason,
            field: None,
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
    ) -> Result<Guarded, Dropped> {
        if !Name::is_valid(name) {
            return Err(DropReason::InvalidEventName.into());
        }
        let def = self.event(name).ok_or(DropReason::UnknownEvent)?;
        guard_fields(def.fields(), properties, true)
    }

    /// Validates identity traits. Every trait is optional.
    pub fn guard_traits(&self, traits: &Map<String, Value>) -> Result<Guarded, Dropped> {
        guard_fields(self.traits(), traits, false)
    }
}

fn guard_fields(
    declared: &BTreeMap<Name, FieldDef>,
    properties: &Map<String, Value>,
    enforce_required: bool,
) -> Result<Guarded, Dropped> {
    if properties.len() > MAX_PROPERTIES {
        return Err(DropReason::TooManyProperties.into());
    }
    // A missing required field is reported before an invalid one, so the
    // reason does not depend on how the fields happen to be named. Of several
    // missing fields, the first by name is reported.
    if enforce_required
        && let Some((name, _)) = declared
            .iter()
            .find(|(name, def)| def.required && !properties.contains_key(name.as_str()))
    {
        return Err(Dropped {
            reason: DropReason::MissingRequiredField,
            field: Some(name.clone()),
        });
    }
    let mut fields = BTreeMap::new();
    let mut present = 0;
    let mut invalid = Vec::new();
    for (name, def) in declared {
        let required = def.required && enforce_required;
        let Some(value) = properties.get(name.as_str()) else {
            continue;
        };
        present += 1;
        match sanitize(&def.kind, value) {
            Some(value) => {
                fields.insert(name.clone(), value);
            }
            None if required => {
                return Err(Dropped {
                    reason: DropReason::InvalidRequiredField,
                    field: Some(name.clone()),
                });
            }
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
/// characters (Cf) that can reorder or hide text, the line and paragraph
/// separators, every Unicode space except the ASCII one (Zs), and the blank
/// fillers that render as nothing (U+115F, U+1160, U+2800, U+3164, U+FFA0), and
/// the invisible combining marks U+034F, U+17B4, U+17B5 and U+180B to U+180D.
/// The standard library has no category lookup, so the sets are listed by
/// hand from the Unicode character database.
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            u32::from(c),
            0x00A0
                | 0x00AD
                | 0x0600..=0x0605
                | 0x061C
                | 0x115F..=0x1160
                | 0x1680
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x180B..=0x180E
                | 0x034F
                | 0x17B4..=0x17B5
                | 0x2000..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0x202F
                | 0x205F
                | 0x2800
                | 0x3000
                | 0x3164
                | 0xFEFF
                | 0xFFA0
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

/// The shape of an `id` value: `[A-Za-z0-9_.:-]`, 1 to 64 characters.
pub(crate) fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_ID_LEN
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// A variation selector changes how the character before it is drawn and is
/// legitimate after an emoji, so `text` keeps it; a value made only of them
/// shows nothing and counts as empty.
fn is_variation_selector(c: char) -> bool {
    matches!(u32::from(c), 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
}

fn sanitize(kind: &FieldKind, value: &Value) -> Option<FieldValue> {
    match kind {
        FieldKind::Id => {
            let text = value.as_str()?;
            is_id(text).then(|| FieldValue::Text(text.to_owned()))
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
            let blank = capped.is_empty() || capped.chars().all(is_variation_selector);
            (!blank).then(|| FieldValue::Text(capped.to_owned()))
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

    fn dropped(reason: DropReason, field: &str) -> Dropped {
        Dropped {
            reason,
            field: Some(name(field)),
        }
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
    fn text_replaces_blank_fillers_and_unicode_spaces() {
        let kind = FieldKind::text(50).unwrap();
        let cases = [
            ("a\u{3164}b", "a b"),
            ("a\u{115F}\u{1160}b", "a  b"),
            ("a\u{FFA0}b", "a b"),
            ("a\u{2800}b", "a b"),
            ("a\u{00A0}b", "a b"),
            ("a\u{3000}b", "a b"),
            ("a\u{2003}b\u{202F}c", "a b c"),
            ("\u{3164}\u{00A0}x", "x"),
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
    fn text_replaces_invisible_combining_marks_but_keeps_variation_selectors() {
        let kind = FieldKind::text(50).unwrap();
        let cases = [
            ("a\u{034F}b", "a b"),
            ("a\u{17B4}\u{17B5}b", "a  b"),
            ("a\u{180B}\u{180C}\u{180D}b", "a   b"),
            ("\u{2764}\u{FE0F}", "\u{2764}\u{FE0F}"),
            ("a\u{FE0E}b", "a\u{FE0E}b"),
            ("x\u{E0100}", "x\u{E0100}"),
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
    fn text_made_only_of_variation_selectors_is_invalid() {
        let kind = FieldKind::text(50).unwrap();
        for input in ["\u{FE0F}", "\u{FE00}\u{FE0F}", "\u{E0100}", " \u{FE0F} "] {
            assert_eq!(guard_one(kind.clone(), json!(input)), None, "{input:?}");
        }
    }
    #[test]
    fn text_made_only_of_invisible_characters_is_invalid() {
        let kind = FieldKind::text(50).unwrap();
        for input in [
            "\u{3164}",
            "\u{2800}\u{00A0}",
            "\u{FFA0}\u{115F}",
            " \u{3000} ",
        ] {
            assert_eq!(guard_one(kind.clone(), json!(input)), None, "{input:?}");
        }
    }

    #[test]
    fn a_dropped_event_names_the_declared_field_that_failed() {
        let def = EventDef::new(BTreeMap::from([
            (name("a"), FieldDef::required(FieldKind::Id)),
            (name("b"), FieldDef::required(FieldKind::Id)),
            (name("c"), FieldDef::optional(FieldKind::Id)),
        ]))
        .unwrap();
        let schema = Schema::new(BTreeMap::from([(name("probe"), def)]), BTreeMap::new()).unwrap();
        // Of two missing required fields, the first by name is reported.
        assert_eq!(
            schema.guard_event("probe", &props(json!({}))),
            Err(dropped(DropReason::MissingRequiredField, "a"))
        );
        assert_eq!(
            schema.guard_event("probe", &props(json!({ "b": "ok" }))),
            Err(dropped(DropReason::MissingRequiredField, "a"))
        );
        // An invalid required field is named; an undeclared one never is.
        assert_eq!(
            schema.guard_event(
                "probe",
                &props(json!({ "a": "ok", "b": "no good", "zzz": 1 }))
            ),
            Err(dropped(DropReason::InvalidRequiredField, "b"))
        );
        // An event that fails the schema itself names no field.
        assert_eq!(
            schema
                .guard_event("other", &props(json!({})))
                .unwrap_err()
                .field,
            None
        );
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
            Err(dropped(DropReason::MissingRequiredField, "f"))
        );
    }

    #[test]
    fn invalid_required_field_drops_the_event() {
        let schema = probe(FieldKind::Id, true);
        assert_eq!(
            schema.guard_event("probe", &props(json!({ "f": "not ok" }))),
            Err(dropped(DropReason::InvalidRequiredField, "f"))
        );
    }

    #[test]
    fn missing_required_field_wins_over_an_invalid_one_whatever_the_names() {
        for (first, second) in [("a", "b"), ("b", "a")] {
            let def = EventDef::new(BTreeMap::from([
                (name(first), FieldDef::required(FieldKind::Id)),
                (name(second), FieldDef::required(FieldKind::Id)),
            ]))
            .unwrap();
            let schema =
                Schema::new(BTreeMap::from([(name("probe"), def)]), BTreeMap::new()).unwrap();
            let properties = props(json!({ first: "not ok" }));
            assert_eq!(
                schema.guard_event("probe", &properties),
                Err(dropped(DropReason::MissingRequiredField, second))
            );
        }
    }

    #[test]
    fn screen_and_experiment_viewed_have_their_fixed_shapes() {
        let schema = Schema::default();
        assert!(
            schema
                .guard_event("screen", &props(json!({ "name": "home" })))
                .is_ok()
        );
        assert_eq!(
            schema.guard_event("screen", &props(json!({ "title": "home" }))),
            Err(dropped(DropReason::MissingRequiredField, "name"))
        );
        let experiment = json!({ "experiment": "new_checkout", "variant": "b" });
        assert_eq!(
            schema
                .guard_event("experiment_viewed", &props(experiment))
                .unwrap()
                .fields
                .len(),
            2
        );
        assert_eq!(
            schema.guard_event("experiment_viewed", &props(json!({ "experiment": "x" }))),
            Err(dropped(DropReason::MissingRequiredField, "variant"))
        );
    }

    #[test]
    fn unknown_event_is_dropped() {
        let schema = probe(FieldKind::Id, true);
        assert_eq!(
            schema.guard_event("other", &props(json!({}))),
            Err(DropReason::UnknownEvent.into())
        );
    }

    #[test]
    fn malformed_event_name_is_dropped() {
        let schema = probe(FieldKind::Id, true);
        for value in ["", "Probe", "pro be", "probe\n", &"a".repeat(65)] {
            assert_eq!(
                schema.guard_event(value, &props(json!({}))),
                Err(DropReason::InvalidEventName.into()),
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
            Err(DropReason::TooManyProperties.into())
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
            Err(dropped(DropReason::MissingRequiredField, "element"))
        );
        assert_eq!(
            schema.guard_event("app_lifecycle", &props(json!({ "state": "sleeping" }))),
            Err(dropped(DropReason::InvalidRequiredField, "state"))
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
            Err(DropReason::UnknownEvent.into())
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
            Err(DropReason::TooManyProperties.into())
        );
    }

    #[test]
    fn drop_reason_all_lists_every_variant() {
        // The match has no wildcard: a new variant stops this from compiling
        // until it is handled here and added to `ALL`.
        for reason in DropReason::ALL {
            match reason {
                DropReason::MalformedEvent
                | DropReason::InvalidEventId
                | DropReason::InvalidTimestamp
                | DropReason::InvalidAnonId
                | DropReason::InvalidSessionId
                | DropReason::InvalidScreen
                | DropReason::InvalidProperties
                | DropReason::InvalidEventName
                | DropReason::UnknownEvent
                | DropReason::TooManyProperties
                | DropReason::MissingRequiredField
                | DropReason::InvalidRequiredField => {}
            }
        }
        let listed: std::collections::BTreeSet<_> = DropReason::ALL.into_iter().collect();
        assert_eq!(listed.len(), 12);
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
            // invisible fillers and Unicode spaces (Zs)
            0x00A0, 0x115F, 0x1160, 0x1680, 0x2000, 0x200A, 0x202F, 0x205F, 0x2800, 0x3000, 0x3164,
            0x034F, 0x17B4, 0x17B5, 0x180B, 0x180C, 0x180D, 0xFFA0,
        ] {
            assert!(flagged(code), "U+{code:04X}");
        }
    }

    #[test]
    fn neighbours_of_the_ranges_are_kept() {
        for code in [
            0x00AC, 0x00AE, 0x05FF, 0x0606, 0x1FFF, 0x2010, 0x2027, 0x2030, 0x205E, 0x2070, 0xFEFE,
            0xFF00, 0xFFF8, 0xFFFC, 0xE0000, 0xE0002, 0xE001F, 0xE0080, 0x0041, 0x00E9, 0x65E5,
            0x00A1, 0x115E, 0x1161, 0x167F, 0x1681, 0x27FF, 0x2801, 0x2FFF, 0x3001, 0x3163, 0x3165,
            0xFF9F, 0xFFA1, 0x0020, 0x034E, 0x0350, 0x17B3, 0x17B6, 0x180A, 0xFE00, 0xFE0F,
            0xE0100,
        ] {
            assert!(!flagged(code), "U+{code:04X}");
        }
    }
}
