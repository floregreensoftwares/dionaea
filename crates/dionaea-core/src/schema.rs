use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::builtin;
use crate::name::{EnumValue, Name};

/// Most fields one event (or the identity traits) may declare, and the most
/// properties one incoming event may carry.
pub const MAX_PROPERTIES: usize = 32;
/// Hard ceiling for a declared `text` maximum, in characters.
pub const MAX_TEXT_CEILING: usize = 4096;
/// Largest declared `count` or `duration` maximum: the biggest integer every
/// JSON implementation represents exactly.
pub const MAX_INTEGER_CEILING: u64 = (1 << 53) - 1;
/// Most values one `enum` may declare.
pub const MAX_ENUM_VALUES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    ReservedEventName(Name),
    RequiredTrait(Name),
    TooManyFields,
    TextMaxOutOfRange,
    IntegerMaxOutOfRange,
    EnumValuesInvalid,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedEventName(name) => write!(f, "event name `{name}` is reserved"),
            Self::RequiredTrait(name) => write!(f, "identity trait `{name}` cannot be required"),
            Self::TooManyFields => write!(f, "at most {MAX_PROPERTIES} fields may be declared"),
            Self::TextMaxOutOfRange => {
                write!(f, "text max must be between 1 and {MAX_TEXT_CEILING}")
            }
            Self::IntegerMaxOutOfRange => {
                write!(
                    f,
                    "count and duration max must be between 1 and {MAX_INTEGER_CEILING}"
                )
            }
            Self::EnumValuesInvalid => write!(
                f,
                "an enum declares between 1 and {MAX_ENUM_VALUES} distinct values"
            ),
        }
    }
}

impl std::error::Error for SchemaError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// Identifier-shaped string: `[A-Za-z0-9_.:-]`, 1 to 64 characters.
    Id,
    /// Free text: control and format characters and line separators become
    /// spaces, trimmed, cut at `max` characters.
    Text { max: usize },
    /// Finite number clamped to 0..1 and rounded to three decimals.
    Unit,
    /// One of the declared values.
    Enum { values: BTreeSet<EnumValue> },
    /// Non-negative integer no greater than `max`.
    Count { max: u64 },
    /// Milliseconds, a non-negative integer no greater than `max`.
    Duration { max: u64 },
}

impl FieldKind {
    pub fn text(max: usize) -> Result<Self, SchemaError> {
        if (1..=MAX_TEXT_CEILING).contains(&max) {
            Ok(Self::Text { max })
        } else {
            Err(SchemaError::TextMaxOutOfRange)
        }
    }

    pub fn enumeration<I: IntoIterator<Item = EnumValue>>(values: I) -> Result<Self, SchemaError> {
        let values: Vec<EnumValue> = values.into_iter().collect();
        let distinct: BTreeSet<EnumValue> = values.iter().cloned().collect();
        if distinct.is_empty() || distinct.len() != values.len() || distinct.len() > MAX_ENUM_VALUES
        {
            return Err(SchemaError::EnumValuesInvalid);
        }
        Ok(Self::Enum { values: distinct })
    }

    /// Re-checks the limits the constructors enforce. The variants are public,
    /// so a kind built by hand is validated again when it joins a schema.
    fn validate(&self) -> Result<(), SchemaError> {
        match self {
            Self::Id | Self::Unit => Ok(()),
            Self::Text { max } => Self::text(*max).map(|_| ()),
            Self::Enum { values } => Self::enumeration(values.iter().cloned()).map(|_| ()),
            Self::Count { max } | Self::Duration { max } => integer_max(*max).map(|_| ()),
        }
    }

    pub fn count(max: u64) -> Result<Self, SchemaError> {
        integer_max(max).map(|max| Self::Count { max })
    }

    pub fn duration(max: u64) -> Result<Self, SchemaError> {
        integer_max(max).map(|max| Self::Duration { max })
    }
}

fn integer_max(max: u64) -> Result<u64, SchemaError> {
    if (1..=MAX_INTEGER_CEILING).contains(&max) {
        Ok(max)
    } else {
        Err(SchemaError::IntegerMaxOutOfRange)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDef {
    pub kind: FieldKind,
    pub required: bool,
}

impl FieldDef {
    pub fn required(kind: FieldKind) -> Self {
        Self {
            kind,
            required: true,
        }
    }

    pub fn optional(kind: FieldKind) -> Self {
        Self {
            kind,
            required: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDef {
    fields: BTreeMap<Name, FieldDef>,
}

impl EventDef {
    pub fn new(fields: BTreeMap<Name, FieldDef>) -> Result<Self, SchemaError> {
        if fields.len() > MAX_PROPERTIES {
            return Err(SchemaError::TooManyFields);
        }
        for def in fields.values() {
            def.kind.validate()?;
        }
        Ok(Self { fields })
    }

    pub fn fields(&self) -> &BTreeMap<Name, FieldDef> {
        &self.fields
    }
}

/// What a site declares: its events and its identity traits. Built-in events
/// are always present and are not part of this value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    events: BTreeMap<Name, EventDef>,
    traits: BTreeMap<Name, FieldDef>,
}

impl Schema {
    /// The rule for what a site may call an event. Shared with the schema
    /// file parser so both report the same thing.
    pub(crate) fn check_event_name(name: &Name) -> Result<(), SchemaError> {
        if builtin::is_reserved(name.as_str()) {
            Err(SchemaError::ReservedEventName(name.clone()))
        } else {
            Ok(())
        }
    }

    pub fn new(
        events: BTreeMap<Name, EventDef>,
        traits: BTreeMap<Name, FieldDef>,
    ) -> Result<Self, SchemaError> {
        for name in events.keys() {
            Self::check_event_name(name)?;
        }
        if traits.len() > MAX_PROPERTIES {
            return Err(SchemaError::TooManyFields);
        }
        if let Some((name, _)) = traits.iter().find(|(_, def)| def.required) {
            return Err(SchemaError::RequiredTrait(name.clone()));
        }
        for def in traits.values() {
            def.kind.validate()?;
        }
        Ok(Self { events, traits })
    }

    pub fn events(&self) -> &BTreeMap<Name, EventDef> {
        &self.events
    }

    pub fn traits(&self) -> &BTreeMap<Name, FieldDef> {
        &self.traits
    }

    /// Built-in events first, then the site's own.
    pub(crate) fn event(&self, name: &str) -> Option<&EventDef> {
        builtin::event(name).or_else(|| self.events.get(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> EnumValue {
        EnumValue::new(text).unwrap()
    }

    fn name(value: &str) -> Name {
        Name::new(value).unwrap()
    }

    #[test]
    fn text_max_must_be_within_bounds() {
        assert!(FieldKind::text(1).is_ok());
        assert!(FieldKind::text(MAX_TEXT_CEILING).is_ok());
        assert_eq!(FieldKind::text(0), Err(SchemaError::TextMaxOutOfRange));
        assert_eq!(
            FieldKind::text(MAX_TEXT_CEILING + 1),
            Err(SchemaError::TextMaxOutOfRange)
        );
    }

    #[test]
    fn integer_max_must_be_within_bounds() {
        assert!(FieldKind::count(1).is_ok());
        assert!(FieldKind::duration(MAX_INTEGER_CEILING).is_ok());
        assert_eq!(FieldKind::count(0), Err(SchemaError::IntegerMaxOutOfRange));
        assert_eq!(
            FieldKind::duration(MAX_INTEGER_CEILING + 1),
            Err(SchemaError::IntegerMaxOutOfRange)
        );
    }

    #[test]
    fn enum_needs_distinct_values_within_bounds() {
        assert!(FieldKind::enumeration([value("a"), value("b")]).is_ok());
        assert_eq!(
            FieldKind::enumeration([]),
            Err(SchemaError::EnumValuesInvalid)
        );
        assert_eq!(
            FieldKind::enumeration([value("a"), value("a")]),
            Err(SchemaError::EnumValuesInvalid)
        );
        let many = (0..=MAX_ENUM_VALUES).map(|i| value(&format!("v{i}")));
        assert_eq!(
            FieldKind::enumeration(many),
            Err(SchemaError::EnumValuesInvalid)
        );
    }

    #[test]
    fn event_declares_at_most_the_property_limit() {
        let fields = |count: usize| -> BTreeMap<Name, FieldDef> {
            (0..count)
                .map(|i| (name(&format!("f{i}")), FieldDef::optional(FieldKind::Id)))
                .collect()
        };
        assert!(EventDef::new(fields(MAX_PROPERTIES)).is_ok());
        assert_eq!(
            EventDef::new(fields(MAX_PROPERTIES + 1)),
            Err(SchemaError::TooManyFields)
        );
    }

    #[test]
    fn site_cannot_declare_a_reserved_event_name() {
        for reserved in ["screen", "ui_tap", "app_error", "web_vital"] {
            let events =
                BTreeMap::from([(name(reserved), EventDef::new(BTreeMap::new()).unwrap())]);
            assert_eq!(
                Schema::new(events, BTreeMap::new()),
                Err(SchemaError::ReservedEventName(name(reserved)))
            );
        }
    }

    #[test]
    fn site_may_declare_its_own_event_names() {
        let events = BTreeMap::from([(name("checkout"), EventDef::new(BTreeMap::new()).unwrap())]);
        assert!(Schema::new(events, BTreeMap::new()).is_ok());
    }

    #[test]
    fn hand_built_kinds_are_validated_when_they_join_a_schema() {
        let bad = [
            (
                FieldKind::Text { max: usize::MAX },
                SchemaError::TextMaxOutOfRange,
            ),
            (
                FieldKind::Count { max: u64::MAX },
                SchemaError::IntegerMaxOutOfRange,
            ),
            (
                FieldKind::Duration { max: 0 },
                SchemaError::IntegerMaxOutOfRange,
            ),
            (
                FieldKind::Enum {
                    values: BTreeSet::new(),
                },
                SchemaError::EnumValuesInvalid,
            ),
        ];
        for (kind, error) in bad {
            let fields = BTreeMap::from([(name("f"), FieldDef::optional(kind.clone()))]);
            assert_eq!(EventDef::new(fields), Err(error.clone()));
            let traits = BTreeMap::from([(name("f"), FieldDef::optional(kind))]);
            assert_eq!(Schema::new(BTreeMap::new(), traits), Err(error));
        }
    }

    #[test]
    fn identity_traits_cannot_be_required() {
        let traits = BTreeMap::from([(name("plan"), FieldDef::required(FieldKind::Id))]);
        assert_eq!(
            Schema::new(BTreeMap::new(), traits),
            Err(SchemaError::RequiredTrait(name("plan")))
        );
    }

    #[test]
    fn traits_declare_at_most_the_property_limit() {
        let traits: BTreeMap<Name, FieldDef> = (0..=MAX_PROPERTIES)
            .map(|i| (name(&format!("t{i}")), FieldDef::optional(FieldKind::Id)))
            .collect();
        assert_eq!(
            Schema::new(BTreeMap::new(), traits),
            Err(SchemaError::TooManyFields)
        );
    }
}
