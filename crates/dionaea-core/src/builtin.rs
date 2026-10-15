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

pub(crate) fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
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

    BTreeMap::from([
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
    fn health_names_are_reserved_but_have_no_shape_yet() {
        assert!(is_reserved("app_error"));
        assert!(event("app_error").is_none());
    }
}
