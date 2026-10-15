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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub severity: Severity,
    /// Where it happened, for example `events.checkout.fields.items`.
    pub path: String,
    pub description: String,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.severity, self.path, self.description)
    }
}

/// Every difference between `old` and `new`, in a stable order. An empty list
/// means the two schemas declare the same thing.
pub fn diff(old: &Schema, new: &Schema) -> Vec<Change> {
    let mut changes = Vec::new();

    for (name, new_event) in new.events() {
        let path = format!("events.{name}");
        match old.events().get(name) {
            None => push(&mut changes, Severity::Allowed, &path, "event added"),
            Some(old_event) => {
                diff_fields(
                    &mut changes,
                    &format!("{path}.fields"),
                    old_event.fields(),
                    new_event.fields(),
                );
            }
        }
    }
    for name in old.events().keys() {
        if !new.events().contains_key(name) {
            push(
                &mut changes,
                Severity::Deprecated,
                &format!("events.{name}"),
                "event removed; older apps may still send it",
            );
        }
    }

    diff_fields(&mut changes, "traits", old.traits(), new.traits());
    changes
}

/// Whether any change in the list is breaking.
pub fn is_breaking(changes: &[Change]) -> bool {
    changes.iter().any(|c| c.severity == Severity::Breaking)
}

fn push(changes: &mut Vec<Change>, severity: Severity, path: &str, description: &str) {
    changes.push(Change {
        severity,
        path: path.to_owned(),
        description: description.to_owned(),
    });
}

fn diff_fields(
    changes: &mut Vec<Change>,
    prefix: &str,
    old: &BTreeMap<Name, FieldDef>,
    new: &BTreeMap<Name, FieldDef>,
) {
    for (name, new_field) in new {
        let path = format!("{prefix}.{name}");
        match old.get(name) {
            None if new_field.required => push(
                changes,
                Severity::Breaking,
                &path,
                "required field added; older apps do not send it",
            ),
            None => push(changes, Severity::Allowed, &path, "optional field added"),
            Some(old_field) => diff_field(changes, &path, old_field, new_field),
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            push(
                changes,
                Severity::Deprecated,
                &format!("{prefix}.{name}"),
                "field removed; older apps may still send it and it will be stripped",
            );
        }
    }
}

fn diff_field(changes: &mut Vec<Change>, path: &str, old: &FieldDef, new: &FieldDef) {
    match (old.required, new.required) {
        (false, true) => push(
            changes,
            Severity::Breaking,
            path,
            "optional field became required",
        ),
        (true, false) => push(
            changes,
            Severity::Allowed,
            path,
            "required field became optional",
        ),
        _ => {}
    }
    diff_kind(changes, path, &old.kind, &new.kind);
}

fn diff_kind(changes: &mut Vec<Change>, path: &str, old: &FieldKind, new: &FieldKind) {
    match (old, new) {
        (FieldKind::Id, FieldKind::Id) | (FieldKind::Unit, FieldKind::Unit) => {}
        (FieldKind::Text { max: old }, FieldKind::Text { max: new }) => {
            compare_max(changes, path, old, new);
        }
        (FieldKind::Count { max: old }, FieldKind::Count { max: new })
        | (FieldKind::Duration { max: old }, FieldKind::Duration { max: new }) => {
            compare_max(changes, path, old, new);
        }
        (FieldKind::Enum { values: old }, FieldKind::Enum { values: new }) => {
            let removed: Vec<&str> = old.difference(new).map(|v| v.as_str()).collect();
            if !removed.is_empty() {
                push(
                    changes,
                    Severity::Breaking,
                    path,
                    &format!("enum values removed: {}", removed.join(", ")),
                );
            }
            let added: Vec<&str> = new.difference(old).map(|v| v.as_str()).collect();
            if !added.is_empty() {
                push(
                    changes,
                    Severity::Allowed,
                    path,
                    &format!("enum values added: {}", added.join(", ")),
                );
            }
        }
        (old, new) => push(
            changes,
            Severity::Breaking,
            path,
            &format!("kind changed from {} to {}", old.name(), new.name()),
        ),
    }
}

fn compare_max<T: PartialOrd + fmt::Display>(
    changes: &mut Vec<Change>,
    path: &str,
    old: &T,
    new: &T,
) {
    if new < old {
        push(
            changes,
            Severity::Breaking,
            path,
            &format!("max narrowed from {old} to {new}"),
        );
    } else if new > old {
        push(
            changes,
            Severity::Allowed,
            path,
            &format!("max widened from {old} to {new}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_schema;

    fn schema(source: &str) -> Schema {
        parse_schema(source).unwrap()
    }

    /// One event `e` with a single field `f` described by `field_toml`, plus
    /// the optional extra lines appended to the file.
    fn with_field(field_toml: &str) -> Schema {
        schema(&format!("[events.e.fields.f]\n{field_toml}\n"))
    }

    fn severities(old: &Schema, new: &Schema) -> Vec<Severity> {
        diff(old, new).into_iter().map(|c| c.severity).collect()
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
        assert_eq!(changes[0].severity, Severity::Allowed);
        assert_eq!(changes[0].path, "events.e");
    }

    #[test]
    fn removing_an_event_is_a_deprecation() {
        let old = with_field("kind = \"id\"");
        let changes = diff(&old, &Schema::default());
        assert_eq!(changes[0].severity, Severity::Deprecated);
        assert_eq!(changes[0].path, "events.e");
        assert!(!is_breaking(&changes));
    }

    #[test]
    fn adding_an_optional_field_is_allowed() {
        let old = schema("[events.e]\n");
        let new = with_field("kind = \"id\"");
        assert_eq!(severities(&old, &new), vec![Severity::Allowed]);
    }

    #[test]
    fn adding_a_required_field_to_an_existing_event_is_breaking() {
        let old = schema("[events.e]\n");
        let new = with_field("kind = \"id\"\nrequired = true");
        assert_eq!(severities(&old, &new), vec![Severity::Breaking]);
    }

    #[test]
    fn removing_a_field_is_a_deprecation() {
        let old = with_field("kind = \"id\"");
        let new = schema("[events.e]\n");
        assert_eq!(severities(&old, &new), vec![Severity::Deprecated]);
    }

    #[test]
    fn making_an_optional_field_required_is_breaking() {
        let old = with_field("kind = \"id\"");
        let new = with_field("kind = \"id\"\nrequired = true");
        assert_eq!(severities(&old, &new), vec![Severity::Breaking]);
    }

    #[test]
    fn making_a_required_field_optional_is_allowed() {
        let old = with_field("kind = \"id\"\nrequired = true");
        let new = with_field("kind = \"id\"");
        assert_eq!(severities(&old, &new), vec![Severity::Allowed]);
    }

    #[test]
    fn changing_a_kind_is_breaking_for_every_pair() {
        let kinds = [
            "kind = \"id\"",
            "kind = \"unit\"",
            "kind = \"text\"\nmax = 10",
            "kind = \"enum\"\nvalues = [\"a\"]",
            "kind = \"count\"\nmax = 10",
            "kind = \"duration\"\nmax = 10",
        ];
        for (i, from) in kinds.iter().enumerate() {
            for (j, to) in kinds.iter().enumerate() {
                if i == j {
                    continue;
                }
                let changes = diff(&with_field(from), &with_field(to));
                assert_eq!(changes.len(), 1, "{from} -> {to}");
                assert_eq!(changes[0].severity, Severity::Breaking, "{from} -> {to}");
                assert!(changes[0].description.contains("kind changed"));
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
    fn removing_enum_values_is_breaking_and_adding_is_allowed() {
        let wide = with_field("kind = \"enum\"\nvalues = [\"a\", \"b\"]");
        let narrow = with_field("kind = \"enum\"\nvalues = [\"a\"]");
        let changes = diff(&wide, &narrow);
        assert_eq!(changes[0].severity, Severity::Breaking);
        assert!(changes[0].description.contains('b'));
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
    fn traits_follow_the_same_rules() {
        let old = schema("[traits.plan]\nkind = \"enum\"\nvalues = [\"free\", \"pro\"]\n");
        let narrowed = schema("[traits.plan]\nkind = \"enum\"\nvalues = [\"free\"]\n");
        let changes = diff(&old, &narrowed);
        assert_eq!(changes[0].severity, Severity::Breaking);
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
}
