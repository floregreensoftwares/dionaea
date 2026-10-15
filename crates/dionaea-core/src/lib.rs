//! Event schema model and privacy guard shared by the Dionaea server and SDKs.
//!
//! This crate has no I/O, no async runtime and no storage dependency, so the
//! same validation runs on the server and inside clients.

mod builtin;
mod diff;
mod guard;
mod name;
mod parse;
mod schema;

pub use diff::{Change, Severity, diff, is_breaking};
pub use guard::{DropReason, FieldValue, Guarded};
pub use name::{EnumValue, InvalidEnumValue, InvalidName, MAX_NAME_LEN, Name};
pub use parse::{Diagnostic, MAX_DIAGNOSTICS, MAX_SOURCE_LEN, parse_schema};
pub use schema::{
    EventDef, FieldDef, FieldKind, MAX_ENUM_VALUES, MAX_INTEGER_CEILING, MAX_PROPERTIES,
    MAX_TEXT_CEILING, Schema, SchemaError,
};
