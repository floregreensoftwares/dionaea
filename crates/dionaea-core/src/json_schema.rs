//! A JSON Schema of the schema file, for editor completion and validation
//! (TOML and JSON share a data model, so editors with TOML support can use
//! it). It describes the same rules `parse_schema` enforces; the tests check
//! that the two agree on a set of documents.

use serde_json::{Value, json};

use crate::builtin;
use crate::name::MAX_NAME_LEN;
use crate::schema::{MAX_ENUM_VALUES, MAX_INTEGER_CEILING, MAX_PROPERTIES, MAX_TEXT_CEILING};

/// The JSON Schema (draft 2020-12) of the schema file format.
pub fn schema_file_json_schema() -> Value {
    let name = format!("^[a-z][a-z0-9_]{{0,{}}}$", MAX_NAME_LEN - 1);
    let enum_value = format!("^[a-z0-9][a-z0-9_.-]{{0,{}}}$", MAX_NAME_LEN - 1);
    let fields = json!({
        "type": "object",
        "maxProperties": MAX_PROPERTIES,
        "propertyNames": { "pattern": name },
        "additionalProperties": { "$ref": "#/$defs/field" }
    });

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Dionaea schema file",
        "description": "The events and identity traits a site declares. Anything not declared is dropped (events) or stripped (fields).",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "events": {
                "description": "Events by name. Built-in and health event names are reserved.",
                "type": "object",
                "propertyNames": {
                    "pattern": name,
                    "not": { "enum": builtin::reserved() }
                },
                "additionalProperties": { "$ref": "#/$defs/event" }
            },
            "traits": {
                "description": "Identity traits by name; all are optional.",
                "type": "object",
                "maxProperties": MAX_PROPERTIES,
                "propertyNames": { "pattern": name },
                "additionalProperties": { "$ref": "#/$defs/trait" }
            }
        },
        "$defs": {
            "event": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "fields": fields }
            },
            "trait": {
                "allOf": [
                    { "$ref": "#/$defs/field" },
                    { "properties": { "required": { "const": false } } }
                ]
            },
            "field": {
                "type": "object",
                "required": ["kind"],
                "additionalProperties": false,
                "properties": {
                    "kind": {
                        "description": "The kind of value the field accepts.",
                        "enum": ["id", "text", "unit", "enum", "count", "duration"]
                    },
                    "required": {
                        "description": "An event without this field is dropped. Default false.",
                        "type": "boolean"
                    },
                    "max": {
                        "description": "text: the longest value in characters (it is cut, not rejected). count, duration: the largest value (milliseconds for duration).",
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_INTEGER_CEILING
                    },
                    "values": {
                        "description": "enum: the accepted values.",
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ENUM_VALUES,
                        "uniqueItems": true,
                        "items": { "type": "string", "pattern": enum_value }
                    }
                },
                "allOf": [
                    {
                        "if": { "required": ["kind"], "properties": { "kind": { "enum": ["id", "unit"] } } },
                        "then": { "not": { "anyOf": [{ "required": ["max"] }, { "required": ["values"] }] } }
                    },
                    {
                        "if": { "required": ["kind"], "properties": { "kind": { "const": "text" } } },
                        "then": {
                            "required": ["max"],
                            "not": { "required": ["values"] },
                            "properties": { "max": { "maximum": MAX_TEXT_CEILING } }
                        }
                    },
                    {
                        "if": { "required": ["kind"], "properties": { "kind": { "enum": ["count", "duration"] } } },
                        "then": { "required": ["max"], "not": { "required": ["values"] } }
                    },
                    {
                        "if": { "required": ["kind"], "properties": { "kind": { "const": "enum" } } },
                        "then": { "required": ["values"], "not": { "required": ["max"] } }
                    }
                ]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_schema;

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
                .map(|i| {
                    format!("{FIELD}kind = \"id\"\n").replace("fields.f", &format!("fields.f{i}"))
                })
                .collect::<String>()
        };
        docs.push(many_fields(crate::MAX_PROPERTIES));
        docs.push(many_fields(crate::MAX_PROPERTIES + 1));
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
    fn the_example_schema_is_accepted() {
        let source = include_str!("../../../examples/schema.toml");
        assert!(schema_accepts(&validator(), source));
    }

    #[test]
    fn every_reserved_event_name_is_rejected() {
        let validator = validator();
        for name in builtin::reserved() {
            assert!(
                !schema_accepts(&validator, &format!("[events.{name}]\n")),
                "{name}"
            );
        }
    }
}
