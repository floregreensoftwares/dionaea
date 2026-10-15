//! Checks that the JSON Schema of the schema file format and `parse_schema`
//! agree. The JSON Schema validator is a test dependency of this crate, not of
//! `dionaea-core`, so building the core (or its benchmarks) never compiles it.

use dionaea_core::{MAX_PROPERTIES, parse_schema, reserved_event_names, schema_file_json_schema};
use serde_json::Value;

fn validator() -> jsonschema::Validator {
    jsonschema::validator_for(&schema_file_json_schema()).expect("the schema compiles")
}

/// Whether the JSON Schema accepts the document, read as TOML.
fn schema_accepts(validator: &jsonschema::Validator, source: &str) -> bool {
    match toml::from_str::<Value>(source) {
        Ok(document) => validator.is_valid(&document),
        Err(_) => false,
    }
}

const FIELD: &str = "[events.e.fields.f]\n";

fn documents() -> Vec<String> {
    let mut docs: Vec<String> = [
        "",
        "[events.e]\n",
        "[traits.t]\nkind = \"id\"\n",
        "[traits.t]\nkind = \"id\"\nrequired = false\n",
        "[traits.t]\nkind = \"id\"\nrequired = true\n",
        "[events.screen]\n",
        "[events.app_error]\n",
        "[events.Bad]\n",
        "[events.a-b]\n",
        "[eventz.a]\n",
        "[events.e.extra]\nx = 1\n",
        "[events.e.fields.F]\nkind = \"id\"\n",
        "[events.e.fields.f]\nkind = \"float\"\n",
        "[events.e.fields.f]\nmax = 3\n",
        "[events.e.fields.f]\nkind = \"id\"\nmax = 3\n",
        "[events.e.fields.f]\nkind = \"id\"\nvalues = [\"a\"]\n",
        "[events.e.fields.f]\nkind = \"unit\"\nmax = 3\n",
        "[events.e.fields.f]\nkind = \"id\"\nrequired = \"yes\"\n",
        "[events.e.fields.f]\nkind = \"id\"\nunknown = 1\n",
        "[events.e.fields.f]\nkind = \"text\"\n",
        "[events.e.fields.f]\nkind = \"text\"\nmax = 0\n",
        "[events.e.fields.f]\nkind = \"text\"\nmax = 4096\n",
        "[events.e.fields.f]\nkind = \"text\"\nmax = 4097\n",
        "[events.e.fields.f]\nkind = \"text\"\nmax = 10\nvalues = [\"a\"]\n",
        "[events.e.fields.f]\nkind = \"count\"\n",
        "[events.e.fields.f]\nkind = \"count\"\nmax = 9007199254740991\n",
        "[events.e.fields.f]\nkind = \"count\"\nmax = 9007199254740992\n",
        "[events.e.fields.f]\nkind = \"count\"\nmax = 1.5\n",
        "[events.e.fields.f]\nkind = \"duration\"\nmax = 60000\n",
        "[events.e.fields.f]\nkind = \"duration\"\nmax = -1\n",
        "[events.e.fields.f]\nkind = \"enum\"\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = []\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = [\"us-east\", \"3d\", \"v1.2\"]\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = [\"a\", \"a\"]\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = [\"En-US\"]\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = [\"-a\"]\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = [1]\n",
        "[events.e.fields.f]\nkind = \"enum\"\nvalues = [\"a\"]\nmax = 3\n",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    // Limits on counts and lengths.
    let many_fields = |count: usize| {
        (0..count)
            .map(|i| format!("{FIELD}kind = \"id\"\n").replace("fields.f", &format!("fields.f{i}")))
            .collect::<String>()
    };
    docs.push(many_fields(MAX_PROPERTIES));
    docs.push(many_fields(MAX_PROPERTIES + 1));
    let long_name = |len: usize| format!("[events.{}]\n", "a".repeat(len));
    docs.push(long_name(64));
    docs.push(long_name(65));
    let many_values = |count: usize| {
        let values: Vec<String> = (0..count).map(|i| format!("\"v{i}\"")).collect();
        format!("{FIELD}kind = \"enum\"\nvalues = [{}]\n", values.join(", "))
    };
    docs.push(many_values(64));
    docs.push(many_values(65));
    let many_traits = |count: usize| {
        (0..count)
            .map(|i| format!("[traits.t{i}]\nkind = \"id\"\n"))
            .collect::<String>()
    };
    docs.push(many_traits(32));
    docs.push(many_traits(33));
    docs
}

#[test]
fn the_json_schema_is_a_valid_json_schema() {
    assert!(jsonschema::meta::is_valid(&schema_file_json_schema()));
}

#[test]
fn the_json_schema_and_the_parser_agree_on_every_document() {
    let validator = validator();
    let mut accepted = 0;
    for source in documents() {
        let parser = parse_schema(&source).is_ok();
        let json_schema = schema_accepts(&validator, &source);
        assert_eq!(
            parser, json_schema,
            "the parser says {parser} and the JSON Schema says {json_schema} for:\n{source}"
        );
        accepted += usize::from(parser);
    }
    // The comparison is only meaningful if both outcomes occur.
    assert!(accepted > 5 && accepted < documents().len() - 5);
}

#[test]
fn the_json_schema_and_the_parser_agree_on_every_combination() {
    let validator = validator();
    let kinds = ["id", "text", "unit", "enum", "count", "duration", "float"];
    let maxes = [
        None,
        Some("0"),
        Some("1"),
        Some("4096"),
        Some("4097"),
        Some("9007199254740991"),
        Some("9007199254740992"),
    ];
    let values = [
        None,
        Some("[]"),
        Some("[\"a\"]"),
        Some("[\"a\", \"a\"]"),
        Some("[\"A\"]"),
    ];
    let requireds = [None, Some("true"), Some("false")];
    let mut checked = 0;
    let mut accepted = 0;
    for kind in kinds {
        for max in maxes {
            for values in values {
                for required in requireds {
                    for header in ["[events.e.fields.f]", "[traits.f]"] {
                        let mut source = format!("{header}\nkind = \"{kind}\"\n");
                        if let Some(max) = max {
                            source.push_str(&format!("max = {max}\n"));
                        }
                        if let Some(values) = values {
                            source.push_str(&format!("values = {values}\n"));
                        }
                        if let Some(required) = required {
                            source.push_str(&format!("required = {required}\n"));
                        }
                        let parser = parse_schema(&source).is_ok();
                        let json_schema = schema_accepts(&validator, &source);
                        assert_eq!(
                            parser, json_schema,
                            "the parser says {parser} and the JSON Schema says {json_schema} for:\n{source}"
                        );
                        checked += 1;
                        accepted += usize::from(parser);
                    }
                }
            }
        }
    }
    assert_eq!(checked, 7 * 7 * 5 * 3 * 2);
    assert!(accepted > 20, "{accepted} accepted of {checked}");
}
#[test]
fn the_example_schema_is_accepted() {
    let source = include_str!("../../../examples/schema.toml");
    assert!(schema_accepts(&validator(), source));
}

#[test]
fn every_reserved_event_name_is_rejected() {
    let validator = validator();
    for name in reserved_event_names() {
        assert!(
            !schema_accepts(&validator, &format!("[events.{name}]\n")),
            "{name}"
        );
    }
}
