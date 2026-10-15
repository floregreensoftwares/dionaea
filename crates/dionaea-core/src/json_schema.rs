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
                    "not": { "enum": builtin::reserved_event_names() }
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
