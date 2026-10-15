//! The schema file: TOML in, a validated [`Schema`] or located diagnostics out.
//!
//! ```toml
//! [events.checkout_started.fields.items]
//! kind = "count"
//! max = 100
//! required = true
//!
//! [events.checkout_started.fields.coupon]
//! kind = "text"
//! max = 32
//!
//! [traits.plan]
//! kind = "enum"
//! values = ["free", "pro"]
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

use serde::Deserialize;
use toml::Spanned;

use crate::builtin;
use crate::name::{EnumValue, Name};
use crate::schema::{EventDef, FieldDef, FieldKind, MAX_PROPERTIES, Schema};

/// Largest schema file accepted, in bytes.
pub const MAX_SOURCE_LEN: usize = 1 << 20;

/// A problem in a schema file, with a 1-based position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.column, self.message)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    events: BTreeMap<Spanned<String>, RawEvent>,
    #[serde(default)]
    traits: BTreeMap<Spanned<String>, RawField>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvent {
    #[serde(default)]
    fields: BTreeMap<Spanned<String>, RawField>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    kind: Spanned<String>,
    required: Option<Spanned<bool>>,
    max: Option<Spanned<u64>>,
    values: Option<Spanned<Vec<Spanned<String>>>>,
}

/// Most diagnostics returned; further problems are summarised in one line.
pub const MAX_DIAGNOSTICS: usize = 100;

/// Parses and validates a schema file. Syntax and shape errors stop at the
/// first problem the TOML reader finds; once the file has the right shape,
/// rule violations are reported in file order, up to [`MAX_DIAGNOSTICS`].
pub fn parse_schema(source: &str) -> Result<Schema, Vec<Diagnostic>> {
    if source.len() > MAX_SOURCE_LEN {
        return Err(vec![Diagnostic {
            line: 1,
            column: 1,
            message: format!("the schema file is larger than {MAX_SOURCE_LEN} bytes"),
        }]);
    }
    let locator = Locator::new(source);
    let raw: RawFile = toml::from_str(source).map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start);
        vec![locator.diagnostic(offset, error.message())]
    })?;

    let mut diagnostics = Diagnostics {
        locator,
        found: Vec::new(),
        omitted: 0,
    };
    let mut events = BTreeMap::new();
    for (key, event) in &raw.events {
        let name = diagnostics.name(key);
        let fields = diagnostics.fields(&event.fields);
        let Some(name) = name else {
            continue;
        };
        if builtin::is_reserved(name.as_str()) {
            diagnostics.report(&key.span(), format!("event name `{name}` is reserved"));
            continue;
        }
        match EventDef::new(fields) {
            Ok(def) => {
                events.insert(name, def);
            }
            Err(error) => diagnostics.report(&key.span(), error.to_string()),
        }
    }
    if raw.traits.len() > MAX_PROPERTIES {
        let mut spans: Vec<Range<usize>> = raw.traits.keys().map(Spanned::span).collect();
        spans.sort_by_key(|span| span.start);
        diagnostics.report(
            &spans[MAX_PROPERTIES],
            format!("at most {MAX_PROPERTIES} identity traits may be declared"),
        );
    }
    let traits = diagnostics.traits(&raw.traits);

    if let Some(found) = diagnostics.finish() {
        return Err(found);
    }
    Schema::new(events, traits).map_err(|error| {
        vec![Diagnostic {
            line: 1,
            column: 1,
            message: error.to_string(),
        }]
    })
}

/// Turns byte offsets into 1-based line and column, with the line starts
/// computed once.
struct Locator<'a> {
    source: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Locator<'a> {
    fn new(source: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        Self {
            source,
            line_starts,
        }
    }

    fn diagnostic(&self, offset: usize, message: &str) -> Diagnostic {
        let mut offset = offset.min(self.source.len());
        while !self.source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.line_starts.partition_point(|&start| start <= offset);
        let start = self.line_starts[line - 1];
        Diagnostic {
            line,
            column: self.source[start..offset].chars().count() + 1,
            message: message.to_owned(),
        }
    }
}

struct Diagnostics<'a> {
    locator: Locator<'a>,
    found: Vec<Diagnostic>,
    omitted: usize,
}

impl Diagnostics<'_> {
    fn report(&mut self, span: &Range<usize>, message: impl AsRef<str>) {
        if self.found.len() >= MAX_DIAGNOSTICS {
            self.omitted += 1;
            return;
        }
        self.found
            .push(self.locator.diagnostic(span.start, message.as_ref()));
    }

    /// The diagnostics in file order, or `None` when there are none.
    fn finish(&mut self) -> Option<Vec<Diagnostic>> {
        if self.found.is_empty() {
            return None;
        }
        let mut found = std::mem::take(&mut self.found);
        found.sort_by_key(|d| (d.line, d.column));
        if self.omitted > 0 {
            found.push(Diagnostic {
                line: found.last().map_or(1, |d| d.line),
                column: 1,
                message: format!("{} more problems not shown", self.omitted),
            });
        }
        Some(found)
    }

    fn name(&mut self, key: &Spanned<String>) -> Option<Name> {
        match Name::new(key.get_ref()) {
            Ok(name) => Some(name),
            Err(error) => {
                self.report(&key.span(), format!("`{}`: {error}", key.get_ref()));
                None
            }
        }
    }

    fn fields(&mut self, raw: &BTreeMap<Spanned<String>, RawField>) -> BTreeMap<Name, FieldDef> {
        let mut fields = BTreeMap::new();
        for (key, field) in raw {
            let name = self.name(key);
            let kind = self.kind(field);
            if let (Some(name), Some(kind)) = (name, kind) {
                let required = field.required.as_ref().is_some_and(|r| *r.get_ref());
                fields.insert(name, FieldDef { kind, required });
            }
        }
        fields
    }

    fn traits(&mut self, raw: &BTreeMap<Spanned<String>, RawField>) -> BTreeMap<Name, FieldDef> {
        for field in raw.values() {
            if let Some(required) = field.required.as_ref().filter(|r| *r.get_ref()) {
                self.report(&required.span(), "identity traits cannot be required");
            }
        }
        self.fields(raw)
    }

    fn kind(&mut self, field: &RawField) -> Option<FieldKind> {
        let kind_name = field.kind.get_ref().as_str();
        let allows_max = matches!(kind_name, "text" | "count" | "duration");
        let allows_values = kind_name == "enum";
        let mut ok = true;
        if !allows_max
            && let Some(max) = &field.max
            && is_known(kind_name)
        {
            self.report(
                &max.span(),
                format!("`max` does not apply to kind `{kind_name}`"),
            );
            ok = false;
        }
        if !allows_values
            && let Some(values) = &field.values
            && is_known(kind_name)
        {
            self.report(
                &values.span(),
                format!("`values` does not apply to kind `{kind_name}`"),
            );
            ok = false;
        }
        let built = match kind_name {
            "id" => Some(FieldKind::Id),
            "unit" => Some(FieldKind::Unit),
            "text" => self.bounded(field, kind_name, |max| {
                usize::try_from(max)
                    .map_err(|_| crate::SchemaError::TextMaxOutOfRange)
                    .and_then(FieldKind::text)
            }),
            "count" => self.bounded(field, kind_name, FieldKind::count),
            "duration" => self.bounded(field, kind_name, FieldKind::duration),
            "enum" => self.enumeration(field),
            other => {
                self.report(
                    &field.kind.span(),
                    format!(
                        "unknown kind `{other}`; expected id, text, unit, enum, count or duration"
                    ),
                );
                None
            }
        };
        built.filter(|_| ok)
    }

    fn bounded(
        &mut self,
        field: &RawField,
        kind_name: &str,
        build: impl FnOnce(u64) -> Result<FieldKind, crate::SchemaError>,
    ) -> Option<FieldKind> {
        let Some(max) = &field.max else {
            self.report(
                &field.kind.span(),
                format!("kind `{kind_name}` needs `max`"),
            );
            return None;
        };
        match build(*max.get_ref()) {
            Ok(kind) => Some(kind),
            Err(error) => {
                self.report(&max.span(), error.to_string());
                None
            }
        }
    }

    fn enumeration(&mut self, field: &RawField) -> Option<FieldKind> {
        let Some(values) = &field.values else {
            self.report(&field.kind.span(), "kind `enum` needs `values`");
            return None;
        };
        let mut parsed = Vec::new();
        let mut ok = true;
        for value in values.get_ref() {
            match EnumValue::new(value.get_ref()) {
                Ok(value) => parsed.push(value),
                Err(error) => {
                    self.report(&value.span(), format!("`{}`: {error}", value.get_ref()));
                    ok = false;
                }
            }
        }
        if !ok {
            return None;
        }
        match FieldKind::enumeration(parsed) {
            Ok(kind) => Some(kind),
            Err(error) => {
                self.report(&values.span(), error.to_string());
                None
            }
        }
    }
}

fn is_known(kind: &str) -> bool {
    matches!(kind, "id" | "text" | "unit" | "enum" | "count" | "duration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::DropReason;
    use serde_json::json;

    fn errors(source: &str) -> Vec<(usize, usize, String)> {
        parse_schema(source)
            .unwrap_err()
            .into_iter()
            .map(|d| (d.line, d.column, d.message))
            .collect()
    }

    const VALID: &str = r#"
[events.checkout_started.fields.items]
kind = "count"
max = 100
required = true

[events.checkout_started.fields.coupon]
kind = "text"
max = 32

[events.checkout_started.fields.region]
kind = "enum"
values = ["us-east", "eu-west"]

[events.checkout_started.fields.progress]
kind = "unit"

[events.checkout_started.fields.load_time]
kind = "duration"
max = 60000

[events.checkout_started.fields.cart]
kind = "id"

[traits.plan]
kind = "enum"
values = ["free", "pro"]
"#;

    #[test]
    fn a_complete_file_parses_into_a_working_schema() {
        let schema = parse_schema(VALID).unwrap();
        let ok = json!({ "items": 3, "coupon": "SAVE", "region": "us-east" });
        let guarded = schema
            .guard_event("checkout_started", ok.as_object().unwrap())
            .unwrap();
        assert_eq!(guarded.fields.len(), 3);
        let missing = json!({ "coupon": "SAVE" });
        assert_eq!(
            schema.guard_event("checkout_started", missing.as_object().unwrap()),
            Err(DropReason::MissingRequiredField)
        );
        let traits = json!({ "plan": "pro" });
        assert_eq!(
            schema
                .guard_traits(traits.as_object().unwrap())
                .unwrap()
                .fields
                .len(),
            1
        );
    }

    #[test]
    fn an_empty_file_is_an_empty_schema() {
        assert_eq!(parse_schema("").unwrap(), Schema::default());
    }

    #[test]
    fn syntax_errors_carry_their_position() {
        let found = errors("[events.a.fields.b]\nkind = \n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 2);
    }

    #[test]
    fn unknown_keys_are_located() {
        let found = errors("[events.a.fields.b]\nkind = \"id\"\nmaxx = 3\n");
        assert_eq!((found[0].0, found[0].1), (3, 1));
        assert!(found[0].2.contains("maxx"), "{}", found[0].2);
    }

    #[test]
    fn an_unknown_top_level_table_is_rejected() {
        let found = errors("[eventz.a]\n");
        assert_eq!((found[0].0, found[0].1), (1, 2));
    }

    #[test]
    fn a_missing_kind_is_reported() {
        let found = errors("[events.a.fields.b]\nmax = 3\n");
        assert_eq!(found.len(), 1);
        assert!(found[0].2.contains("kind"), "{}", found[0].2);
    }

    #[test]
    fn an_unknown_kind_points_at_the_value() {
        let found = errors("[events.a.fields.b]\nkind = \"float\"\n");
        assert_eq!((found[0].0, found[0].1), (2, 8));
        assert!(found[0].2.contains("unknown kind `float`"));
    }

    #[test]
    fn invalid_names_point_at_the_key() {
        let found = errors("[events.Checkout.fields.b]\nkind = \"id\"\n");
        assert_eq!(found[0].0, 1);
        assert!(found[0].2.contains("Checkout"), "{}", found[0].2);
        let found = errors("[events.a.fields.\"b c\"]\nkind = \"id\"\n");
        assert_eq!(found[0].0, 1);
    }

    #[test]
    fn reserved_event_names_point_at_the_event() {
        let found = errors("\n[events.screen.fields.b]\nkind = \"id\"\n");
        assert_eq!(found[0].0, 2);
        assert!(found[0].2.contains("reserved"));
        assert_eq!(errors("[events.app_error]\n")[0].0, 1);
    }

    #[test]
    fn text_needs_a_max_within_bounds() {
        let found = errors("[events.a.fields.b]\nkind = \"text\"\n");
        assert!(found[0].2.contains("needs `max`"));
        let found = errors("[events.a.fields.b]\nkind = \"text\"\nmax = 0\n");
        assert_eq!((found[0].0, found[0].1), (3, 7));
        let found = errors("[events.a.fields.b]\nkind = \"text\"\nmax = 99999\n");
        assert_eq!(found[0].0, 3);
    }

    #[test]
    fn count_and_duration_need_a_max_within_bounds() {
        for kind in ["count", "duration"] {
            let found = errors(&format!("[events.a.fields.b]\nkind = \"{kind}\"\n"));
            assert!(found[0].2.contains("needs `max`"));
            let found = errors(&format!(
                "[events.a.fields.b]\nkind = \"{kind}\"\nmax = 9007199254740993\n"
            ));
            assert_eq!(found[0].0, 3);
        }
    }

    #[test]
    fn enum_needs_valid_distinct_values() {
        let found = errors("[events.a.fields.b]\nkind = \"enum\"\n");
        assert!(found[0].2.contains("needs `values`"));
        let found = errors("[events.a.fields.b]\nkind = \"enum\"\nvalues = []\n");
        assert_eq!(found[0].0, 3);
        let found = errors("[events.a.fields.b]\nkind = \"enum\"\nvalues = [\"a\", \"a\"]\n");
        assert_eq!(found[0].0, 3);
        let found = errors("[events.a.fields.b]\nkind = \"enum\"\nvalues = [\"ok\", \"En-US\"]\n");
        assert_eq!((found[0].0, found[0].1), (3, 17));
    }

    #[test]
    fn attributes_that_do_not_apply_to_the_kind_are_rejected() {
        let found = errors("[events.a.fields.b]\nkind = \"id\"\nmax = 3\n");
        assert_eq!((found[0].0, found[0].1), (3, 7));
        assert!(found[0].2.contains("does not apply"));
        let found = errors("[events.a.fields.b]\nkind = \"count\"\nmax = 3\nvalues = [\"a\"]\n");
        assert_eq!(found[0].0, 4);
    }

    #[test]
    fn identity_traits_cannot_be_required() {
        let found = errors("[traits.plan]\nkind = \"id\"\nrequired = true\n");
        assert_eq!((found[0].0, found[0].1), (3, 12));
        assert!(parse_schema("[traits.plan]\nkind = \"id\"\nrequired = false\n").is_ok());
    }

    #[test]
    fn an_event_may_not_declare_too_many_fields() {
        let mut source = String::from("[events.a]\n");
        for i in 0..=crate::MAX_PROPERTIES {
            source.push_str(&format!("[events.a.fields.f{i}]\nkind = \"id\"\n"));
        }
        let found = errors(&source);
        assert_eq!(found[0].0, 1);
        assert!(found[0].2.contains("at most"));
    }

    #[test]
    fn every_violation_in_a_well_formed_file_is_reported() {
        let source =
            "[events.a.fields.b]\nkind = \"float\"\n\n[events.a.fields.c]\nkind = \"text\"\n";
        let found = errors(source);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].0, found[1].0), (2, 5));
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        let found = errors("# é\n[events.a.fields.b]\nkind = \"nope\"\n");
        assert_eq!((found[0].0, found[0].1), (3, 8));
        let found = errors("[events.a.fields.b]\nkind = \"é\"\n");
        assert_eq!((found[0].0, found[0].1), (2, 8));
    }

    #[test]
    fn diagnostics_are_in_file_order_not_key_order() {
        let source = "[events.zeta.fields.b]\nkind = \"float\"\n\n[events.alpha.fields.b]\nkind = \"nope\"\n";
        let lines: Vec<usize> = errors(source).iter().map(|d| d.0).collect();
        assert_eq!(lines, vec![2, 5]);
    }

    #[test]
    fn too_many_traits_are_located_at_the_first_one_past_the_limit() {
        let mut source = String::new();
        for i in 0..=crate::MAX_PROPERTIES {
            source.push_str(&format!("[traits.t{i:02}]\nkind = \"id\"\n"));
        }
        let found = errors(&source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, crate::MAX_PROPERTIES * 2 + 1);
        assert!(found[0].2.contains("identity traits"));
    }

    #[test]
    fn errors_inside_an_event_with_a_bad_name_are_still_reported() {
        let found = errors("[events.Checkout.fields.x]\nkind = \"float\"\n");
        let lines: Vec<usize> = found.iter().map(|d| d.0).collect();
        assert_eq!(lines, vec![1, 2]);
    }

    #[test]
    fn the_number_of_diagnostics_is_capped() {
        let mut source = String::new();
        for i in 0..(MAX_DIAGNOSTICS * 3) {
            source.push_str(&format!("[traits.t{i}]\nkind = \"nope\"\n"));
        }
        let found = errors(&source);
        assert_eq!(found.len(), MAX_DIAGNOSTICS + 1);
        assert!(found.last().unwrap().2.contains("more problems"));
    }

    #[test]
    fn oversized_files_are_refused_before_parsing() {
        let source = " ".repeat(MAX_SOURCE_LEN + 1);
        let found = errors(&source);
        assert_eq!(found.len(), 1);
        assert!(found[0].2.contains("larger than"));
    }
}
