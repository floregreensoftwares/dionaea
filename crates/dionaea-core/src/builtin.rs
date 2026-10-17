use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::name::{EnumValue, Name};
use crate::schema::{EventDef, FieldDef, FieldKind};

/// Names a site can never declare: the built-in events and the app health
/// events, whose shapes are defined server-side.
const RESERVED: &[&str] = &[
    "screen",
    "ui_tap",
    "app_lifecycle",
    "experiment_viewed",
    "app_error",
    "app_start",
    "screen_load",
    "http_request",
    "web_vital",
    "frame_stats",
    "anr",
    "app_crash",
];

/// Longest error message kept, before scrubbing, in characters.
pub const MAX_MESSAGE_CHARS: usize = 512;
/// Longest `frames` text of an error, in characters.
pub const MAX_FRAMES_CHARS: usize = 4096;
/// Longest host of a network call, in characters.
const MAX_HOST_CHARS: usize = 128;
/// Longest route template, in characters.
pub const MAX_ROUTE_CHARS: usize = 256;
/// Longest duration of a health event: ten minutes, in milliseconds.
const MAX_DURATION_MS: u64 = 600_000;
/// Largest request or response size: 4 GiB.
const MAX_BYTES: u64 = 1 << 32;
/// Largest web vital value: an hour, in milliseconds.
const MAX_VITAL: u64 = 3_600_000;

pub(crate) fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}
pub fn reserved_event_names() -> &'static [&'static str] {
    RESERVED
}

pub(crate) fn event(name: &str) -> Option<&'static EventDef> {
    BUILTIN.get(name)
}

static BUILTIN: LazyLock<BTreeMap<&'static str, EventDef>> = LazyLock::new(|| {
    let id = || FieldDef::required(FieldKind::Id);
    let def = |fields: &[(&str, FieldDef)]| {
        let fields = fields
            .iter()
            .map(|(name, field)| (n(name), field.clone()))
            .collect();
        EventDef::new(fields).expect("built-in events declare few fields")
    };
    let lifecycle =
        FieldKind::enumeration(["launch", "foreground", "background", "terminate"].map(v))
            .expect("built-in enum is valid");

    let required = |kind: FieldKind| FieldDef::required(kind);
    let optional = |kind: FieldKind| FieldDef::optional(kind);
    let words = |values: &[&str]| {
        FieldKind::enumeration(values.iter().copied().map(v)).expect("built-in enum is valid")
    };
    let text = |max| FieldKind::text(max).expect("built-in text maximum is valid");
    let duration = || FieldKind::duration(MAX_DURATION_MS).expect("built-in maximum is valid");
    let count = |max| FieldKind::count(max).expect("built-in maximum is valid");

    BTreeMap::from([
        // App health. The shapes are fixed here, and the server applies more
        // rules to some fields (scrubbing a message, templating a route)
        // after the guard.
        (
            "app_error",
            def(&[
                (
                    "kind",
                    required(words(&["handled", "unhandled", "boundary", "crash"])),
                ),
                ("type", id()),
                ("message", optional(text(MAX_MESSAGE_CHARS))),
                ("frames", optional(text(MAX_FRAMES_CHARS))),
            ]),
        ),
        (
            "app_start",
            def(&[
                ("start", required(words(&["cold", "warm"]))),
                ("first_frame", required(duration())),
                ("interactive", optional(duration())),
            ]),
        ),
        (
            "screen_load",
            def(&[
                ("render", required(duration())),
                ("interactive", optional(duration())),
            ]),
        ),
        (
            "http_request",
            def(&[
                (
                    "method",
                    required(words(&[
                        "get", "post", "put", "patch", "delete", "head", "options", "other",
                    ])),
                ),
                ("host", required(text(MAX_HOST_CHARS))),
                ("route", required(text(MAX_ROUTE_CHARS))),
                ("status", required(count(599))),
                ("duration", required(duration())),
                ("request_size", optional(count(MAX_BYTES))),
                ("response_size", optional(count(MAX_BYTES))),
                (
                    "connection",
                    optional(words(&["wifi", "cellular", "ethernet", "other", "none"])),
                ),
            ]),
        ),
        (
            "web_vital",
            def(&[
                (
                    "name",
                    required(words(&["lcp", "inp", "cls", "ttfb", "fcp"])),
                ),
                // Milliseconds; a layout shift score in thousandths.
                ("value", required(count(MAX_VITAL))),
                (
                    "rating",
                    optional(words(&["good", "needs_improvement", "poor"])),
                ),
                ("route", optional(text(MAX_ROUTE_CHARS))),
            ]),
        ),
        ("screen", def(&[("name", id())])),
        ("ui_tap", def(&[("screen", id()), ("element", id())])),
        (
            "app_lifecycle",
            def(&[("state", FieldDef::required(lifecycle))]),
        ),
        (
            "experiment_viewed",
            def(&[("experiment", id()), ("variant", id())]),
        ),
    ])
});

fn v(value: &str) -> EnumValue {
    EnumValue::new(value).expect("built-in enum values are valid")
}

fn n(value: &str) -> Name {
    Name::new(value).expect("built-in names are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_event_is_reserved() {
        for name in BUILTIN.keys() {
            assert!(is_reserved(name), "{name}");
        }
    }

    #[test]
    fn builtin_events_resolve() {
        for name in ["screen", "ui_tap", "app_lifecycle", "experiment_viewed"] {
            assert!(event(name).is_some(), "{name}");
        }
    }

    #[test]
    fn health_events_have_a_shape_except_the_ones_not_built_yet() {
        for name in [
            "app_error",
            "app_start",
            "screen_load",
            "http_request",
            "web_vital",
        ] {
            assert!(is_reserved(name), "{name}");
            assert!(event(name).is_some(), "{name}");
        }
        for name in ["frame_stats", "anr", "app_crash"] {
            assert!(is_reserved(name), "{name}");
            assert!(event(name).is_none(), "{name}");
        }
    }
}
