//! Event schema model and privacy guard shared by the Dionaea server and SDKs.
//!
//! This crate has no I/O, no async runtime and no storage dependency, so the
//! same validation runs on the server and inside clients.

mod builtin;
mod diff;
mod guard;
mod json_schema;
mod name;
mod parse;
mod schema;
mod wire;

pub use builtin::reserved_event_names;
pub use diff::{Change, ChangeKind, Severity, diff, is_breaking};
pub use guard::{DropReason, Dropped, FieldValue, Guarded, is_id};
pub use json_schema::schema_file_json_schema;
pub use name::{EnumValue, InvalidEnumValue, InvalidName, MAX_NAME_LEN, Name};
pub use parse::{Diagnostic, MAX_DIAGNOSTICS, MAX_SOURCE_LEN, parse_schema};
pub use schema::{
    EventDef, FieldDef, FieldKind, MAX_ENUM_VALUES, MAX_INTEGER_CEILING, MAX_PROPERTIES,
    MAX_TEXT_CEILING, Schema, SchemaError,
};
pub use wire::{
    Batch, EventId, Identify, MAX_BATCH_EVENTS, MAX_BODY_LEN, MAX_LABEL_LEN, MAX_SCREEN_LEN,
    MAX_TIMESTAMP, Rejected, RequestError, WireEvent, decode_batch, decode_identify,
};
