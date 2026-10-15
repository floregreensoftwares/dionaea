//! Schema evolution: what changed between two published versions, and
//! whether each change is safe for apps already in the wild.
//!
//! An event is accepted when it is valid under any version that is not
//! retired, so most changes cannot lose data. A change is breaking when a
//! version would reject or reshape events that an older version admits and
//! that old apps keep sending.

use std::collections::BTreeMap;
use std::fmt;

use crate::name::Name;
use crate::schema::{FieldDef, FieldKind, Schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Safe: nothing an older app sends is affected.
    Allowed,
    /// Safe, but something is going away; flagged until it is retired.
    Deprecated,
    /// Rejected: an older app's events would be dropped or reshaped.
    Breaking,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Allowed => "allowed",
            Self::Deprecated => "deprecated",
            Self::Breaking => "breaking",
        })
    }
}

/// What happened, with the data a report needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    EventAdded,
    EventRemoved,
    FieldAdded {
        required: bool,
    },
    FieldRemoved,
    BecameRequired,
    BecameOptional,
    KindChanged {
        from: &'static str,
        to: &'static str,
    },
    /// The declared `max` of a text, count or duration field.
    MaxChanged {
        from: u64,
        to: u64,
        rejects: bool,
    },
    EnumValuesRemoved(Vec<String>),
    EnumValuesAdded(Vec<String>),
}

impl ChangeKind {
    pub fn severity(&self) -> Severity {
        match self {
            Self::EventAdded
            | Self::BecameOptional
            | Self::FieldAdded { required: false }
            | Self::EnumValuesAdded(_) => Severity::Allowed,
            Self::EventRemoved | Self::FieldRemoved => Severity::Deprecated,
            Self::FieldAdded { required: true }
            | Self::BecameRequired
            | Self::KindChanged { .. }
            | Self::EnumValuesRemoved(_) => Severity::Breaking,
            Self::MaxChanged { from, to, .. } if to < from => Severity::Breaking,
            Self::MaxChanged { .. } => Severity::Allowed,
        }
    }
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventAdded => f.write_str("event added"),
            Self::EventRemoved => f.write_str("event removed; older apps may still send it"),
            Self::FieldAdded { required: true } => {
                f.write_str("required field added; older apps do not send it")
            }
            Self::FieldAdded { required: false } => f.write_str("optional field added"),
            Self::FieldRemoved => {
                f.write_str("field removed; older apps may still send it and it will be stripped")
            }
            Self::BecameRequired => f.write_str("optional field became required"),
            Self::BecameOptional => f.write_str("required field became optional"),
            Self::KindChanged { from, to } => write!(f, "kind changed from {from} to {to}"),
            Self::MaxChanged { from, to, rejects } if to < from => {
                let effect = if *rejects {
                    "larger values are now rejected"
                } else {
                    "longer text is now cut shorter"
                };
                write!(f, "max narrowed from {from} to {to}; {effect}")
            }
            Self::MaxChanged { from, to, .. } => write!(f, "max widened from {from} to {to}"),
            Self::EnumValuesRemoved(values) => {
                write!(f, "enum values removed: {}", values.join(", "))
            }
            Self::EnumValuesAdded(values) => {
                write!(f, "enum values added: {}", values.join(", "))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Where it happened, for example `events.checkout.fields.items`.
    pub path: String,
    pub kind: ChangeKind,
}

impl Change {
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.severity(), self.path, self.kind)
    }
}

/// Every difference between `old` and `new`: events present in `new` first
/// (by name), then events only in `old`, then traits. An empty list means the
/// two schemas declare the same thing.
pub fn diff(old: &Schema, new: &Schema) -> Vec<Change> {
    let mut changes = Vec::new();

    for (name, new_event) in new.events() {
        match old.events().get(name) {
            None => changes.push(Change {
                path: format!("events.{name}"),
                kind: ChangeKind::EventAdded,
            }),
            Some(old_event) if old_event != new_event => diff_fields(
                &mut changes,
                &format!("events.{name}.fields"),
                old_event.fields(),
                new_event.fields(),
            ),
            Some(_) => {}
        }
    }
    for name in old.events().keys() {
        if !new.events().contains_key(name) {
            changes.push(Change {
                path: format!("events.{name}"),
                kind: ChangeKind::EventRemoved,
            });
        }
    }

    diff_fields(&mut changes, "traits", old.traits(), new.traits());
    changes
}

/// Whether any change in the list is breaking.
pub fn is_breaking(changes: &[Change]) -> bool {
    changes.iter().any(|c| c.severity() == Severity::Breaking)
}

fn diff_fields(
    changes: &mut Vec<Change>,
    prefix: &str,
    old: &BTreeMap<Name, FieldDef>,
    new: &BTreeMap<Name, FieldDef>,
) {
    let path = |name: &Name| format!("{prefix}.{name}");
    for (name, new_field) in new {
        match old.get(name) {
            None => changes.push(Change {
                path: path(name),
                kind: ChangeKind::FieldAdded {
                    required: new_field.required,
                },
            }),
            Some(old_field) if old_field != new_field => {
                diff_field(changes, &path(name), old_field, new_field);
            }
            Some(_) => {}
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            changes.push(Change {
                path: path(name),
                kind: ChangeKind::FieldRemoved,
            });
        }
    }
}

fn diff_field(changes: &mut Vec<Change>, path: &str, old: &FieldDef, new: &FieldDef) {
    let mut report = |kind| {
        changes.push(Change {
            path: path.to_owned(),
            kind,
        });
    };
    match (old.required, new.required) {
        (false, true) => report(ChangeKind::BecameRequired),
        (true, false) => report(ChangeKind::BecameOptional),
        _ => {}
    }
    match (&old.kind, &new.kind) {
        (FieldKind::Id, FieldKind::Id) | (FieldKind::Unit, FieldKind::Unit) => {}
        (FieldKind::Text { max: old }, FieldKind::Text { max: new }) => {
            if old != new {
                report(ChangeKind::MaxChanged {
                    from: *old as u64,
                    to: *new as u64,
                    rejects: false,
                });
            }
        }
        (FieldKind::Count { max: old }, FieldKind::Count { max: new })
        | (FieldKind::Duration { max: old }, FieldKind::Duration { max: new }) => {
            if old != new {
                report(ChangeKind::MaxChanged {
                    from: *old,
                    to: *new,
                    rejects: true,
                });
            }
        }
        (FieldKind::Enum { values: old }, FieldKind::Enum { values: new }) => {
            let removed: Vec<String> = old.difference(new).map(|v| v.as_str().to_owned()).collect();
            let added: Vec<String> = new.difference(old).map(|v| v.as_str().to_owned()).collect();
            if !removed.is_empty() {
                report(ChangeKind::EnumValuesRemoved(removed));
            }
            if !added.is_empty() {
                report(ChangeKind::EnumValuesAdded(added));
            }
        }
        (old, new) => report(ChangeKind::KindChanged {
            from: old.name(),
            to: new.name(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_schema;

    fn schema(source: &str) -> Schema {
        parse_schema(source).unwrap()
    }

    /// One event `e` with a single field `f` described by `field_toml`.
    fn with_field(field_toml: &str) -> Schema {
        schema(&format!("[events.e.fields.f]\n{field_toml}\n"))
    }

    fn severities(old: &Schema, new: &Schema) -> Vec<Severity> {
        diff(old, new).iter().map(Change::severity).collect()
    }

    #[test]
    fn identical_schemas_have_no_changes() {
        let s = with_field("kind = \"count\"\nmax = 10\nrequired = true");
        assert!(diff(&s, &s).is_empty());
        assert!(diff(&Schema::default(), &Schema::default()).is_empty());
    }

    #[test]
    fn adding_an_event_is_allowed() {
        let new = schema("[events.e.fields.f]\nkind = \"id\"\nrequired = true\n");
        let changes = diff(&Schema::default(), &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::EventAdded);
        assert_eq!(changes[0].severity(), Severity::Allowed);
        assert_eq!(changes[0].path, "events.e");
    }

    #[test]
    fn removing_an_event_is_a_deprecation() {
        let old = with_field("kind = \"id\"");
        let changes = diff(&old, &Schema::default());
        assert_eq!(changes[0].kind, ChangeKind::EventRemoved);
        assert_eq!(changes[0].severity(), Severity::Deprecated);
        assert!(!is_breaking(&changes));
    }

    #[test]
    fn adding_an_optional_field_is_allowed() {
        let old = schema("[events.e]\n");
        let new = with_field("kind = \"id\"");
        assert_eq!(
            diff(&old, &new)[0].kind,
            ChangeKind::FieldAdded { required: false }
        );
        assert_eq!(severities(&old, &new), vec![Severity::Allowed]);
    }

    #[test]
    fn adding_a_required_field_to_an_existing_event_is_breaking() {
        let old = schema("[events.e]\n");
        let new = with_field("kind = \"id\"\nrequired = true");
        assert_eq!(
            diff(&old, &new)[0].kind,
            ChangeKind::FieldAdded { required: true }
        );
        assert_eq!(severities(&old, &new), vec![Severity::Breaking]);
    }

    #[test]
    fn removing_a_field_is_a_deprecation() {
        let old = with_field("kind = \"id\"");
        let new = schema("[events.e]\n");
        assert_eq!(diff(&old, &new)[0].kind, ChangeKind::FieldRemoved);
        assert_eq!(severities(&old, &new), vec![Severity::Deprecated]);
    }

    #[test]
    fn making_an_optional_field_required_is_breaking() {
        let old = with_field("kind = \"id\"");
        let new = with_field("kind = \"id\"\nrequired = true");
        assert_eq!(diff(&old, &new)[0].kind, ChangeKind::BecameRequired);
        assert_eq!(severities(&old, &new), vec![Severity::Breaking]);
    }

    #[test]
    fn making_a_required_field_optional_is_allowed() {
        let old = with_field("kind = \"id\"\nrequired = true");
        let new = with_field("kind = \"id\"");
        assert_eq!(diff(&old, &new)[0].kind, ChangeKind::BecameOptional);
        assert_eq!(severities(&old, &new), vec![Severity::Allowed]);
    }

    #[test]
    fn changing_a_kind_is_breaking_for_every_pair() {
        let kinds = [
            ("id", "kind = \"id\""),
            ("unit", "kind = \"unit\""),
            ("text", "kind = \"text\"\nmax = 10"),
            ("enum", "kind = \"enum\"\nvalues = [\"a\"]"),
            ("count", "kind = \"count\"\nmax = 10"),
            ("duration", "kind = \"duration\"\nmax = 10"),
        ];
        for (from_name, from) in kinds {
            for (to_name, to) in kinds {
                if from_name == to_name {
                    continue;
                }
                let changes = diff(&with_field(from), &with_field(to));
                assert_eq!(changes.len(), 1, "{from_name} -> {to_name}");
                assert_eq!(
                    changes[0].kind,
                    ChangeKind::KindChanged {
                        from: from_name,
                        to: to_name
                    }
                );
                assert_eq!(changes[0].severity(), Severity::Breaking);
            }
        }
    }

    #[test]
    fn narrowing_a_max_is_breaking_and_widening_is_allowed() {
        for kind in ["text", "count", "duration"] {
            let small = with_field(&format!("kind = \"{kind}\"\nmax = 10"));
            let large = with_field(&format!("kind = \"{kind}\"\nmax = 20"));
            assert_eq!(
                severities(&large, &small),
                vec![Severity::Breaking],
                "{kind}"
            );
            assert_eq!(
                severities(&small, &large),
                vec![Severity::Allowed],
                "{kind}"
            );
            assert!(diff(&small, &small).is_empty());
        }
    }

    #[test]
    fn the_report_says_whether_a_narrower_max_rejects_or_cuts() {
        let text = diff(
            &with_field("kind = \"text\"\nmax = 20"),
            &with_field("kind = \"text\"\nmax = 10"),
        );
        assert!(text[0].to_string().contains("cut shorter"), "{}", text[0]);
        let count = diff(
            &with_field("kind = \"count\"\nmax = 20"),
            &with_field("kind = \"count\"\nmax = 10"),
        );
        assert!(count[0].to_string().contains("rejected"), "{}", count[0]);
    }

    #[test]
    fn removing_enum_values_is_breaking_and_adding_is_allowed() {
        let wide = with_field("kind = \"enum\"\nvalues = [\"a\", \"b\"]");
        let narrow = with_field("kind = \"enum\"\nvalues = [\"a\"]");
        let changes = diff(&wide, &narrow);
        assert_eq!(
            changes[0].kind,
            ChangeKind::EnumValuesRemoved(vec!["b".to_owned()])
        );
        assert_eq!(changes[0].severity(), Severity::Breaking);
        assert_eq!(severities(&narrow, &wide), vec![Severity::Allowed]);
    }

    #[test]
    fn replacing_an_enum_value_reports_both_sides() {
        let old = with_field("kind = \"enum\"\nvalues = [\"a\", \"b\"]");
        let new = with_field("kind = \"enum\"\nvalues = [\"a\", \"c\"]");
        assert_eq!(
            severities(&old, &new),
            vec![Severity::Breaking, Severity::Allowed]
        );
    }

    #[test]
    fn one_field_changing_in_several_ways_reports_each() {
        let old = with_field("kind = \"count\"\nmax = 10");
        let new = with_field("kind = \"text\"\nmax = 10\nrequired = true");
        let kinds: Vec<ChangeKind> = diff(&old, &new).into_iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ChangeKind::BecameRequired,
                ChangeKind::KindChanged {
                    from: "count",
                    to: "text"
                }
            ]
        );
        assert!(is_breaking(&diff(&old, &new)));
    }

    #[test]
    fn a_new_event_with_required_fields_is_one_allowed_change() {
        let old = schema("[events.a]\n");
        let new = schema(
            "[events.a]\n[events.b.fields.x]\nkind = \"id\"\nrequired = true\n[events.b.fields.y]\nkind = \"id\"\nrequired = true\n",
        );
        let changes = diff(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "events.b");
        assert_eq!(changes[0].severity(), Severity::Allowed);
    }

    #[test]
    fn traits_follow_the_same_rules() {
        let old = schema("[traits.plan]\nkind = \"enum\"\nvalues = [\"free\", \"pro\"]\n");
        let narrowed = schema("[traits.plan]\nkind = \"enum\"\nvalues = [\"free\"]\n");
        let changes = diff(&old, &narrowed);
        assert_eq!(changes[0].severity(), Severity::Breaking);
        assert_eq!(changes[0].path, "traits.plan");
        let added = schema(
            "[traits.plan]\nkind = \"enum\"\nvalues = [\"free\", \"pro\"]\n[traits.seats]\nkind = \"count\"\nmax = 5\n",
        );
        assert_eq!(severities(&old, &added), vec![Severity::Allowed]);
        assert_eq!(severities(&added, &old), vec![Severity::Deprecated]);
    }

    #[test]
    fn several_changes_are_reported_in_a_stable_order() {
        let old = schema(
            "[events.a.fields.x]\nkind = \"id\"\n[events.b.fields.y]\nkind = \"count\"\nmax = 9\n",
        );
        let new = schema("[events.a.fields.x]\nkind = \"unit\"\n[events.c]\n");
        let paths: Vec<String> = diff(&old, &new).into_iter().map(|c| c.path).collect();
        assert_eq!(paths, vec!["events.a.fields.x", "events.c", "events.b"]);
        assert!(is_breaking(&diff(&old, &new)));
    }

    #[test]
    fn display_names_severity_path_and_effect() {
        let changes = diff(&schema("[events.e]\n"), &with_field("kind = \"id\""));
        assert_eq!(
            changes[0].to_string(),
            "allowed: events.e.fields.f: optional field added"
        );
    }
}
