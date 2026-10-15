//! The request bodies of `/v1/collect` and `/v1/identify`, read from bytes.
//!
//! The reader is strict about the envelope and forgiving about events: a
//! malformed envelope rejects the whole request, while a malformed event is
//! set aside with a [`DropReason`] and the rest of the batch goes on. Unknown
//! fields are ignored, so a newer SDK can add fields without breaking an
//! older server. Nothing here looks at the schema; that is the guard's job.

use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::guard::{DropReason, is_id};
use crate::name::Name;
use crate::schema::MAX_PROPERTIES;

/// Largest request body read, in bytes. Checked before anything is parsed.
pub const MAX_BODY_LEN: usize = 256 * 1024;
/// Most events in one batch.
pub const MAX_BATCH_EVENTS: usize = 100;
/// Longest `sdk` and `release` label, in characters.
pub const MAX_LABEL_LEN: usize = 64;
/// Longest `screen`, in characters.
pub const MAX_SCREEN_LEN: usize = 128;
/// Largest timestamp: the biggest integer every JSON implementation keeps
/// exactly. Milliseconds since the Unix epoch.
pub const MAX_TIMESTAMP: u64 = (1 << 53) - 1;

/// Why a whole request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    BodyTooLarge,
    NotJson,
    NotAnObject,
    /// A required envelope field is missing or has the wrong shape.
    InvalidField(&'static str),
    TooManyEvents,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyTooLarge => write!(f, "the body is larger than {MAX_BODY_LEN} bytes"),
            Self::NotJson => f.write_str("the body is not valid JSON"),
            Self::NotAnObject => f.write_str("the body is not a JSON object"),
            Self::InvalidField(name) => write!(f, "`{name}` is missing or invalid"),
            Self::TooManyEvents => write!(f, "a batch holds at most {MAX_BATCH_EVENTS} events"),
        }
    }
}

impl std::error::Error for RequestError {}

impl RequestError {
    /// The name used for counters and in the conformance vectors.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BodyTooLarge => "body_too_large",
            Self::NotJson => "not_json",
            Self::NotAnObject => "not_an_object",
            Self::InvalidField(_) => "invalid_field",
            Self::TooManyEvents => "too_many_events",
        }
    }

    /// The envelope field behind an `InvalidField`.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidField(field) => Some(field),
            _ => None,
        }
    }
}

/// A UUID version 7 (time-ordered), kept as 128 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(u128);

impl EventId {
    /// Parses the canonical 36-character form, either case. Only version 7
    /// with the RFC 4122 variant is accepted.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        let mut value: u128 = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return None;
                }
                continue;
            }
            let digit = char::from(byte).to_digit(16)?;
            value = (value << 4) | u128::from(digit);
        }
        let version = (value >> 76) & 0xF;
        let variant = (value >> 62) & 0x3;
        (version == 7 && variant == 0b10).then_some(Self(value))
    }

    pub fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (v >> 96) as u32,
            (v >> 80) as u16,
            (v >> 64) as u16,
            (v >> 48) as u16,
            v & 0xFFFF_FFFF_FFFF
        )
    }
}

/// One event of a batch, shape-checked but not yet checked against a schema.
#[derive(Debug, Clone, PartialEq)]
pub struct WireEvent {
    pub event_id: EventId,
    /// Client time, milliseconds since the Unix epoch.
    pub ts: u64,
    pub name: String,
    pub anon_id: String,
    pub session_id: Option<String>,
    pub screen: Option<String>,
    pub props: Map<String, Value>,
}

/// An event that was set aside, by its position in the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejected {
    pub index: usize,
    pub reason: DropReason,
}

/// A decoded `/v1/collect` body.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// Client time when the batch was sent, for skew correction.
    pub sent_at: u64,
    /// The SDK that sent it, for example `dionaea-rn/0.1.0`.
    pub sdk: String,
    /// The app version, for example `1.4.2+57`.
    pub release: Option<String>,
    pub events: Vec<WireEvent>,
    pub rejected: Vec<Rejected>,
}

/// A decoded `/v1/identify` body.
#[derive(Debug, Clone, PartialEq)]
pub struct Identify {
    pub sent_at: u64,
    pub sdk: String,
    pub anon_id: String,
    pub user_id: String,
    pub traits: Map<String, Value>,
}

// Reading. The body is read straight into small typed values, never into a
// generic tree: scalars keep only what the checks need, nested values are
// consumed without being stored, and arrays and objects stop growing at their
// limits. A 256 KiB body therefore costs about as much whatever its shape.
//
// Every visitor below answers "any JSON value". The `skip!` macro writes the
// methods that map a shape to one fixed answer (arrays and objects are read
// to the end first, so the document stays valid); each visitor spells out
// only the shapes it actually uses.

macro_rules! skip {
    ($value:expr; $($shape:ident)*) => { $( skip!(@one $value, $shape); )* };
    (@one $value:expr, unit) => {
        fn visit_unit<E>(self) -> Result<Self::Value, E> { Ok($value) }
    };
    (@one $value:expr, bool) => {
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> { Ok($value) }
    };
    (@one $value:expr, i64) => {
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> { Ok($value) }
    };
    (@one $value:expr, u64) => {
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> { Ok($value) }
    };
    (@one $value:expr, f64) => {
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> { Ok($value) }
    };
    (@one $value:expr, str) => {
        fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> { Ok($value) }
    };
    (@one $value:expr, seq) => {
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok($value)
        }
    };
    (@one $value:expr, map) => {
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok($value)
        }
    };
}

/// Makes a type deserialize through its visitor, as "any JSON value".
macro_rules! from_any {
    ($ty:ident, $visitor:expr) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_any($visitor)
            }
        }
    };
}

/// A JSON value reduced to what the checks look at.
enum Scalar {
    Str(String),
    Uint(u64),
    Other,
}

struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    skip!(Scalar::Other; unit bool i64 f64 seq map);
    fn visit_u64<E>(self, value: u64) -> Result<Scalar, E> {
        Ok(Scalar::Uint(value))
    }
    fn visit_str<E>(self, value: &str) -> Result<Scalar, E> {
        Ok(Scalar::Str(value.to_owned()))
    }
    fn visit_string<E>(self, value: String) -> Result<Scalar, E> {
        Ok(Scalar::Str(value))
    }
}

from_any!(Scalar, ScalarVisitor);

/// A property value. Arrays and objects are replaced by an empty array, since
/// no field kind accepts them.
struct PropValue(Value);

struct PropValueVisitor;

impl<'de> Visitor<'de> for PropValueVisitor {
    type Value = PropValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    skip!(PropValue(Value::Null); unit);
    skip!(PropValue(Value::Array(Vec::new())); seq map);
    fn visit_bool<E>(self, value: bool) -> Result<PropValue, E> {
        Ok(PropValue(Value::Bool(value)))
    }
    fn visit_i64<E>(self, value: i64) -> Result<PropValue, E> {
        Ok(PropValue(Value::from(value)))
    }
    fn visit_u64<E>(self, value: u64) -> Result<PropValue, E> {
        Ok(PropValue(Value::from(value)))
    }
    fn visit_f64<E>(self, value: f64) -> Result<PropValue, E> {
        Ok(PropValue(Value::from(value)))
    }
    fn visit_str<E>(self, value: &str) -> Result<PropValue, E> {
        Ok(PropValue(Value::String(value.to_owned())))
    }
    fn visit_string<E>(self, value: String) -> Result<PropValue, E> {
        Ok(PropValue(Value::String(value)))
    }
}

from_any!(PropValue, PropValueVisitor);

/// The `props` or `traits` object: at most [`MAX_PROPERTIES`] entries are
/// stored, the rest are consumed without allocating.
enum Props {
    Map(Map<String, Value>),
    TooMany,
    Other,
}

struct PropsVisitor;

impl<'de> Visitor<'de> for PropsVisitor {
    type Value = Props;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    skip!(Props::Other; unit bool i64 u64 f64 str seq);
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Props, A::Error> {
        let mut stored = Map::new();
        let mut entries = 0usize;
        loop {
            if entries < MAX_PROPERTIES {
                let Some((key, value)) = map.next_entry::<String, PropValue>()? else {
                    break;
                };
                stored.insert(key, value.0);
            } else if map.next_entry::<IgnoredAny, IgnoredAny>()?.is_none() {
                break;
            }
            entries += 1;
        }
        Ok(if entries > MAX_PROPERTIES {
            Props::TooMany
        } else {
            Props::Map(stored)
        })
    }
}

from_any!(Props, PropsVisitor);

/// An element of `events`: an object read as a [`RawEvent`], or anything else.
enum Element {
    Event(RawEvent),
    NotObject,
}

struct ElementVisitor;

impl<'de> Visitor<'de> for ElementVisitor {
    type Value = Element;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    skip!(Element::NotObject; unit bool i64 u64 f64 str seq);
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Element, A::Error> {
        RawEvent::deserialize(MapAccessDeserializer::new(map)).map(Element::Event)
    }
}

from_any!(Element, ElementVisitor);

/// The `events` field: at most [`MAX_BATCH_EVENTS`] elements are stored, and
/// the rest are consumed and reported as an overflow.
enum Events {
    Array { items: Vec<Element>, overflow: bool },
    Other,
}

struct EventsVisitor;

impl<'de> Visitor<'de> for EventsVisitor {
    type Value = Events;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    skip!(Events::Other; unit bool i64 u64 f64 str map);
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Events, A::Error> {
        let mut items = Vec::new();
        while items.len() < MAX_BATCH_EVENTS {
            match seq.next_element::<Element>()? {
                Some(element) => items.push(element),
                None => {
                    return Ok(Events::Array {
                        items,
                        overflow: false,
                    });
                }
            }
        }
        let overflow = seq.next_element::<IgnoredAny>()?.is_some();
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Events::Array { items, overflow })
    }
}

from_any!(Events, EventsVisitor);

/// A top-level body: an object read as `T`, or anything else.
enum Top<T> {
    Object(T),
    NotObject,
}

struct TopVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for TopVisitor<T> {
    type Value = Top<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    skip!(Top::NotObject; unit bool i64 u64 f64 str seq);
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Top<T>, A::Error> {
        T::deserialize(MapAccessDeserializer::new(map)).map(Top::Object)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Top<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TopVisitor(PhantomData))
    }
}

#[derive(Deserialize)]
struct RawBatch {
    sent_at: Option<Scalar>,
    sdk: Option<Scalar>,
    release: Option<Scalar>,
    events: Option<Events>,
}

#[derive(Deserialize)]
struct RawEvent {
    event_id: Option<Scalar>,
    name: Option<Scalar>,
    ts: Option<Scalar>,
    anon_id: Option<Scalar>,
    session_id: Option<Scalar>,
    screen: Option<Scalar>,
    props: Option<Props>,
}

#[derive(Deserialize)]
struct RawIdentify {
    sent_at: Option<Scalar>,
    sdk: Option<Scalar>,
    anon_id: Option<Scalar>,
    user_id: Option<Scalar>,
    traits: Option<Props>,
}

fn read<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, RequestError> {
    if body.len() > MAX_BODY_LEN {
        return Err(RequestError::BodyTooLarge);
    }
    match serde_json::from_slice::<Top<T>>(body) {
        Ok(Top::Object(object)) => Ok(object),
        Ok(Top::NotObject) => Err(RequestError::NotAnObject),
        Err(_) => Err(RequestError::NotJson),
    }
}

fn label(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_LABEL_LEN
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/'))
}

fn screen(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_SCREEN_LEN && text.chars().all(|c| c.is_ascii_graphic())
}

fn timestamp(value: Option<Scalar>) -> Option<u64> {
    match value {
        Some(Scalar::Uint(ts)) if ts <= MAX_TIMESTAMP => Some(ts),
        _ => None,
    }
}

/// A required string that passes `valid`.
fn required(
    value: Option<Scalar>,
    valid: impl Fn(&str) -> bool,
    field: &'static str,
) -> Result<String, RequestError> {
    match value {
        Some(Scalar::Str(text)) if valid(&text) => Ok(text),
        _ => Err(RequestError::InvalidField(field)),
    }
}

/// An optional string that passes `valid` when present.
fn optional(
    value: Option<Scalar>,
    valid: impl Fn(&str) -> bool,
    field: &'static str,
) -> Result<Option<String>, RequestError> {
    match value {
        None => Ok(None),
        other => required(other, valid, field).map(Some),
    }
}

/// Decodes a `/v1/collect` body.
pub fn decode_batch(body: &[u8]) -> Result<Batch, RequestError> {
    let raw: RawBatch = read(body)?;
    let sent_at = timestamp(raw.sent_at).ok_or(RequestError::InvalidField("sent_at"))?;
    let sdk = required(raw.sdk, label, "sdk")?;
    let release = optional(raw.release, label, "release")?;
    let (items, overflow) = match raw.events {
        Some(Events::Array { items, overflow }) => (items, overflow),
        _ => return Err(RequestError::InvalidField("events")),
    };
    if overflow {
        return Err(RequestError::TooManyEvents);
    }

    let mut accepted = Vec::with_capacity(items.len());
    let mut rejected = Vec::new();
    for (index, element) in items.into_iter().enumerate() {
        match decode_event(element) {
            Ok(event) => accepted.push(event),
            Err(reason) => rejected.push(Rejected { index, reason }),
        }
    }
    Ok(Batch {
        sent_at,
        sdk,
        release,
        events: accepted,
        rejected,
    })
}

fn decode_event(element: Element) -> Result<WireEvent, DropReason> {
    let Element::Event(raw) = element else {
        return Err(DropReason::MalformedEvent);
    };
    let event_id = match raw.event_id {
        Some(Scalar::Str(text)) => EventId::parse(&text),
        _ => None,
    }
    .ok_or(DropReason::InvalidEventId)?;
    let name =
        required(raw.name, Name::is_valid, "name").map_err(|_| DropReason::InvalidEventName)?;
    let ts = timestamp(raw.ts).ok_or(DropReason::InvalidTimestamp)?;
    let anon_id = required(raw.anon_id, is_id, "anon_id").map_err(|_| DropReason::InvalidAnonId)?;
    let session_id =
        optional(raw.session_id, is_id, "session_id").map_err(|_| DropReason::InvalidSessionId)?;
    let screen = optional(raw.screen, screen, "screen").map_err(|_| DropReason::InvalidScreen)?;
    let props = match raw.props {
        None => Map::new(),
        Some(Props::Map(props)) => props,
        Some(Props::TooMany) => return Err(DropReason::TooManyProperties),
        Some(Props::Other) => return Err(DropReason::InvalidProperties),
    };
    Ok(WireEvent {
        event_id,
        ts,
        name,
        anon_id,
        session_id,
        screen,
        props,
    })
}

/// Decodes a `/v1/identify` body.
pub fn decode_identify(body: &[u8]) -> Result<Identify, RequestError> {
    let raw: RawIdentify = read(body)?;
    let sent_at = timestamp(raw.sent_at).ok_or(RequestError::InvalidField("sent_at"))?;
    let sdk = required(raw.sdk, label, "sdk")?;
    let anon_id = required(raw.anon_id, is_id, "anon_id")?;
    let user_id = required(raw.user_id, is_id, "user_id")?;
    let traits = match raw.traits {
        None => Map::new(),
        Some(Props::Map(traits)) => traits,
        Some(_) => return Err(RequestError::InvalidField("traits")),
    };
    Ok(Identify {
        sent_at,
        sdk,
        anon_id,
        user_id,
        traits,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: &str = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";

    fn event() -> Value {
        json!({ "event_id": ID, "name": "checkout", "ts": 1760000000000u64, "anon_id": "a1" })
    }

    fn batch(events: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "sent_at": 1760000001000u64, "sdk": "dionaea-rn/0.1.0", "release": "1.4.2+57",
            "events": events
        }))
        .unwrap()
    }

    fn with(mut base: Value, key: &str, value: Value) -> Value {
        base[key] = value;
        base
    }

    fn without(mut base: Value, key: &str) -> Value {
        base.as_object_mut().unwrap().remove(key);
        base
    }

    // EventId

    #[test]
    fn event_id_round_trips_in_lowercase() {
        let id = EventId::parse(&ID.to_uppercase()).unwrap();
        assert_eq!(id.to_string(), ID);
        assert_eq!(EventId::parse(ID), Some(id));
    }

    #[test]
    fn event_id_rejects_other_versions_variants_and_shapes() {
        for bad in [
            "0190f7e2-7c3e-4a52-9a0e-5f3c2a1b4d6e",  // version 4
            "0190f7e2-7c3e-7a52-0a0e-5f3c2a1b4d6e",  // variant 0
            "0190f7e27c3e7a529a0e5f3c2a1b4d6e",      // no hyphens
            "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6",   // too short
            "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e0", // too long
            "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6g",  // not hex
            "",
        ] {
            assert_eq!(EventId::parse(bad), None, "{bad}");
        }
    }

    // Requests

    #[test]
    fn a_valid_batch_is_decoded() {
        let decoded =
            decode_batch(&batch(vec![with(event(), "screen", json!("/orders/:id"))])).unwrap();
        assert_eq!(decoded.sent_at, 1760000001000);
        assert_eq!(decoded.sdk, "dionaea-rn/0.1.0");
        assert_eq!(decoded.release.as_deref(), Some("1.4.2+57"));
        assert!(decoded.rejected.is_empty());
        let event = &decoded.events[0];
        assert_eq!(event.event_id.to_string(), ID);
        assert_eq!(event.name, "checkout");
        assert_eq!(event.screen.as_deref(), Some("/orders/:id"));
        assert_eq!(event.session_id, None);
        assert!(event.props.is_empty());
    }

    #[test]
    fn an_empty_batch_is_valid() {
        let decoded = decode_batch(&batch(vec![])).unwrap();
        assert!(decoded.events.is_empty() && decoded.rejected.is_empty());
    }

    #[test]
    fn unknown_fields_are_ignored_at_both_levels() {
        let body = serde_json::to_vec(&json!({
            "sent_at": 1, "sdk": "s", "future": true,
            "events": [with(event(), "future_field", json!([1, 2]))]
        }))
        .unwrap();
        let decoded = decode_batch(&body).unwrap();
        assert_eq!(decoded.events.len(), 1);
        assert!(decoded.events[0].props.is_empty());
    }

    #[test]
    fn the_body_size_is_checked_before_parsing() {
        let body = vec![b' '; MAX_BODY_LEN + 1];
        assert_eq!(decode_batch(&body), Err(RequestError::BodyTooLarge));
        assert_eq!(decode_identify(&body), Err(RequestError::BodyTooLarge));
    }

    #[test]
    fn bodies_that_are_not_objects_are_refused() {
        assert_eq!(decode_batch(b"not json"), Err(RequestError::NotJson));
        assert_eq!(decode_batch(b""), Err(RequestError::NotJson));
        assert_eq!(decode_batch(b"[]"), Err(RequestError::NotAnObject));
        assert_eq!(decode_batch(b"3"), Err(RequestError::NotAnObject));
    }

    #[test]
    fn deeply_nested_bodies_are_refused_without_overflowing() {
        let body = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(decode_batch(body.as_bytes()).is_err());
        // Nesting inside a property value and in an unknown field is read past
        // without being stored, at any depth the body limit allows.
        let depth = 60_000;
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let in_props = format!(
            "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{{\"event_id\":\"{ID}\",\"name\":\"e\",\"ts\":1,\"anon_id\":\"a\",\"props\":{{\"k\":{nested}}}}}],\"other\":{nested}}}"
        );
        let decoded = decode_batch(in_props.as_bytes()).unwrap();
        assert_eq!(decoded.events.len(), 1);
    }

    #[test]
    fn envelope_fields_are_required_and_checked() {
        let ok = |sent_at: Value, sdk: Value, release: Value, events: Value| {
            let body = serde_json::to_vec(&json!({
                "sent_at": sent_at, "sdk": sdk, "release": release, "events": events
            }))
            .unwrap();
            decode_batch(&body)
        };
        assert!(ok(json!(1), json!("s"), json!(null), json!([])).is_ok());
        assert_eq!(
            ok(json!("1"), json!("s"), json!(null), json!([])),
            Err(RequestError::InvalidField("sent_at"))
        );
        assert_eq!(
            ok(json!(-1), json!("s"), json!(null), json!([])),
            Err(RequestError::InvalidField("sent_at"))
        );
        assert_eq!(
            ok(json!(MAX_TIMESTAMP + 1), json!("s"), json!(null), json!([])),
            Err(RequestError::InvalidField("sent_at"))
        );
        assert_eq!(
            ok(json!(1), json!(null), json!(null), json!([])),
            Err(RequestError::InvalidField("sdk"))
        );
        assert_eq!(
            ok(json!(1), json!("bad sdk"), json!(null), json!([])),
            Err(RequestError::InvalidField("sdk"))
        );
        assert_eq!(
            ok(json!(1), json!("s"), json!("1 2"), json!([])),
            Err(RequestError::InvalidField("release"))
        );
        assert_eq!(
            ok(json!(1), json!("s"), json!(null), json!({})),
            Err(RequestError::InvalidField("events"))
        );
        let long = "a".repeat(MAX_LABEL_LEN + 1);
        assert_eq!(
            ok(json!(1), json!(long), json!(null), json!([])),
            Err(RequestError::InvalidField("sdk"))
        );
    }

    #[test]
    fn a_batch_is_limited_to_the_event_maximum() {
        let events = |count: usize| batch((0..count).map(|_| event()).collect());
        assert!(decode_batch(&events(MAX_BATCH_EVENTS)).is_ok());
        assert_eq!(
            decode_batch(&events(MAX_BATCH_EVENTS + 1)),
            Err(RequestError::TooManyEvents)
        );
    }

    // Events set aside

    fn reason_of(bad: Value) -> DropReason {
        let decoded = decode_batch(&batch(vec![bad])).unwrap();
        assert!(decoded.events.is_empty());
        assert_eq!(decoded.rejected.len(), 1);
        decoded.rejected[0].reason
    }

    #[test]
    fn a_malformed_event_is_set_aside_and_the_rest_goes_on() {
        let decoded =
            decode_batch(&batch(vec![event(), json!("nope"), event(), json!(5)])).unwrap();
        assert_eq!(decoded.events.len(), 2);
        assert_eq!(
            decoded.rejected,
            vec![
                Rejected {
                    index: 1,
                    reason: DropReason::MalformedEvent
                },
                Rejected {
                    index: 3,
                    reason: DropReason::MalformedEvent
                },
            ]
        );
    }

    #[test]
    fn every_event_field_has_its_own_reason() {
        assert_eq!(
            reason_of(without(event(), "event_id")),
            DropReason::InvalidEventId
        );
        assert_eq!(
            reason_of(with(event(), "event_id", json!(7))),
            DropReason::InvalidEventId
        );
        assert_eq!(
            reason_of(with(
                event(),
                "event_id",
                json!("0190f7e2-7c3e-4a52-9a0e-5f3c2a1b4d6e")
            )),
            DropReason::InvalidEventId
        );
        assert_eq!(
            reason_of(without(event(), "name")),
            DropReason::InvalidEventName
        );
        assert_eq!(
            reason_of(with(event(), "name", json!("Bad Name"))),
            DropReason::InvalidEventName
        );
        assert_eq!(
            reason_of(without(event(), "ts")),
            DropReason::InvalidTimestamp
        );
        assert_eq!(
            reason_of(with(event(), "ts", json!(-5))),
            DropReason::InvalidTimestamp
        );
        assert_eq!(
            reason_of(with(event(), "ts", json!(1.5))),
            DropReason::InvalidTimestamp
        );
        assert_eq!(
            reason_of(with(event(), "ts", json!("1"))),
            DropReason::InvalidTimestamp
        );
        assert_eq!(
            reason_of(without(event(), "anon_id")),
            DropReason::InvalidAnonId
        );
        assert_eq!(
            reason_of(with(event(), "anon_id", json!("a b"))),
            DropReason::InvalidAnonId
        );
        assert_eq!(
            reason_of(with(event(), "session_id", json!("a b"))),
            DropReason::InvalidSessionId
        );
        assert_eq!(
            reason_of(with(event(), "session_id", json!(3))),
            DropReason::InvalidSessionId
        );
        assert_eq!(
            reason_of(with(event(), "screen", json!("has space"))),
            DropReason::InvalidScreen
        );
        assert_eq!(
            reason_of(with(event(), "screen", json!("é"))),
            DropReason::InvalidScreen
        );
        assert_eq!(
            reason_of(with(
                event(),
                "screen",
                json!("a".repeat(MAX_SCREEN_LEN + 1))
            )),
            DropReason::InvalidScreen
        );
        assert_eq!(
            reason_of(with(event(), "props", json!([1]))),
            DropReason::InvalidProperties
        );
        assert_eq!(
            reason_of(with(event(), "props", json!("x"))),
            DropReason::InvalidProperties
        );
    }

    #[test]
    fn the_first_failing_check_names_the_reason() {
        // Every field bad: the event id is checked first, then the others in
        // the documented order as each is repaired.
        let mut bad = json!({
            "event_id": "x", "name": "Bad", "ts": -1, "anon_id": "a b",
            "session_id": "s s", "screen": "has space", "props": 1
        });
        let order = [
            ("event_id", json!(ID), DropReason::InvalidEventId),
            ("name", json!("ok"), DropReason::InvalidEventName),
            ("ts", json!(1), DropReason::InvalidTimestamp),
            ("anon_id", json!("a"), DropReason::InvalidAnonId),
            ("session_id", json!("s"), DropReason::InvalidSessionId),
            ("screen", json!("/s"), DropReason::InvalidScreen),
            ("props", json!({}), DropReason::InvalidProperties),
        ];
        for (field, fixed, reason) in order {
            assert_eq!(reason_of(bad.clone()), reason, "before fixing {field}");
            bad[field] = fixed;
        }
        let decoded = decode_batch(&batch(vec![bad])).unwrap();
        assert_eq!(decoded.events.len(), 1);
    }

    #[test]
    fn a_body_of_tiny_properties_is_dropped_without_storing_them() {
        let mut props = String::from("{");
        let mut index = 0;
        while props.len() < MAX_BODY_LEN - 400 {
            props.push_str(&format!("\"k{index}\":1,"));
            index += 1;
        }
        props.push_str("\"last\":1}");
        let body = format!(
            "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{{\"event_id\":\"{ID}\",\"name\":\"e\",\"ts\":1,\"anon_id\":\"a\",\"props\":{props}}}]}}"
        );
        assert!(body.len() <= MAX_BODY_LEN);
        let decoded = decode_batch(body.as_bytes()).unwrap();
        assert!(decoded.events.is_empty());
        assert_eq!(
            decoded.rejected,
            vec![Rejected {
                index: 0,
                reason: DropReason::TooManyProperties
            }]
        );
    }

    #[test]
    fn a_repeated_key_refuses_the_whole_request() {
        let body = format!(
            "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{{\"event_id\":\"{ID}\",\"name\":\"a\",\"name\":\"b\",\"ts\":1,\"anon_id\":\"a\"}}]}}"
        );
        assert_eq!(decode_batch(body.as_bytes()), Err(RequestError::NotJson));
    }

    #[test]
    fn many_tiny_elements_are_refused_as_too_many_events() {
        let body = format!(
            "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{}1]}}",
            "1,".repeat(100_000)
        );
        assert!(body.len() <= MAX_BODY_LEN);
        assert_eq!(
            decode_batch(body.as_bytes()),
            Err(RequestError::TooManyEvents)
        );
    }
    #[test]
    fn optional_event_fields_may_be_absent_or_null() {
        let base = with(
            with(
                with(event(), "session_id", json!(null)),
                "screen",
                json!(null),
            ),
            "props",
            json!(null),
        );
        let decoded = decode_batch(&batch(vec![base])).unwrap();
        assert_eq!(decoded.events.len(), 1);
        assert_eq!(decoded.events[0].session_id, None);
    }

    #[test]
    fn properties_are_passed_through_for_the_guard() {
        let props = json!({ "items": 3, "coupon": "SAVE" });
        let decoded = decode_batch(&batch(vec![with(event(), "props", props.clone())])).unwrap();
        assert_eq!(&Value::Object(decoded.events[0].props.clone()), &props);
    }

    #[test]
    fn a_decoded_event_feeds_the_guard() {
        let schema =
            crate::parse_schema("[events.checkout.fields.items]\nkind = \"count\"\nmax = 9\n")
                .unwrap();
        let decoded = decode_batch(&batch(vec![with(
            event(),
            "props",
            json!({"items": 3, "x": 1}),
        )]))
        .unwrap();
        let wire = &decoded.events[0];
        let guarded = schema.guard_event(&wire.name, &wire.props).unwrap();
        assert_eq!((guarded.fields.len(), guarded.undeclared), (1, 1));
    }

    // Identify

    #[test]
    fn a_valid_identify_is_decoded() {
        let body = serde_json::to_vec(&json!({
            "sent_at": 5, "sdk": "dionaea-web/0.1.0", "anon_id": "a1", "user_id": "u_42",
            "traits": { "plan": "pro" }
        }))
        .unwrap();
        let identify = decode_identify(&body).unwrap();
        assert_eq!(
            (identify.anon_id.as_str(), identify.user_id.as_str()),
            ("a1", "u_42")
        );
        assert_eq!(identify.traits.get("plan"), Some(&json!("pro")));
    }

    #[test]
    fn identify_requires_shaped_ids_and_object_traits() {
        let make = |anon: Value, user: Value, traits: Value| {
            decode_identify(
                &serde_json::to_vec(&json!({
                    "sent_at": 1, "sdk": "s", "anon_id": anon, "user_id": user, "traits": traits
                }))
                .unwrap(),
            )
        };
        assert!(make(json!("a"), json!("u"), json!(null)).is_ok());
        assert_eq!(
            make(json!(null), json!("u"), json!({})),
            Err(RequestError::InvalidField("anon_id"))
        );
        assert_eq!(
            make(json!("a"), json!("a@b.c"), json!({})),
            Err(RequestError::InvalidField("user_id"))
        );
        assert_eq!(
            make(json!("a"), json!("u"), json!([1])),
            Err(RequestError::InvalidField("traits"))
        );
    }

    #[test]
    fn identify_traits_feed_the_guard() {
        let schema =
            crate::parse_schema("[traits.plan]\nkind = \"enum\"\nvalues = [\"free\", \"pro\"]\n")
                .unwrap();
        let body = serde_json::to_vec(&json!({
            "sent_at": 1, "sdk": "s", "anon_id": "a", "user_id": "u",
            "traits": { "plan": "pro", "email": "a@b.c" }
        }))
        .unwrap();
        let identify = decode_identify(&body).unwrap();
        let guarded = schema.guard_traits(&identify.traits).unwrap();
        assert_eq!((guarded.fields.len(), guarded.undeclared), (1, 1));
    }
}
