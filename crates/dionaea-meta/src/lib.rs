//! Metadata for Dionaea: organizations, sites, write keys and published
//! schema versions, behind the [`MetaStore`] trait. [`SqliteMeta`] is the
//! implementation for a single node; all metadata SQL lives in this crate.
//!
//! The ingest path never queries this store per request. It loads a
//! [`Snapshot`] into memory and asks [`MetaStore::generation`] whether
//! anything changed: every change to what a snapshot holds (a key issued or
//! revoked, a schema version published or retired) increases the generation
//! in the same transaction, so a cache that sees the same number holds the
//! same data.
//!
//! Write keys are public identifiers: they ship inside app builds, so they are
//! stored as they are. Their protection is the guard, the limits and the
//! origin allow-list kept here.

use std::collections::BTreeMap;
use std::fmt;

use dionaea_core::{Change, Diagnostic, Schema};

mod migrations;
mod sqlite;
#[cfg(test)]
mod tests;

pub use sqlite::SqliteMeta;

/// Most origins one write key may allow.
pub const MAX_ORIGINS: usize = 32;
/// Longest organization, site or key label, in characters.
pub const MAX_LABEL_LEN: usize = 64;
/// Longest setting name and value, in bytes.
pub const MAX_SETTING_NAME_LEN: usize = 64;
pub const MAX_SETTING_VALUE_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub id: u32,
    pub organization_id: u32,
    pub name: String,
}

/// A write key as listed for administration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteKeyInfo {
    pub key: String,
    pub site_id: u32,
    pub label: String,
    /// Empty for native apps: no origin check is made.
    pub origins: Vec<String>,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

/// A schema version as listed for administration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaVersionInfo {
    pub site_id: u32,
    pub version: u32,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}

/// The result of publishing a schema file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub version: u32,
    /// What changed against the previous active version; empty for the first.
    pub changes: Vec<Change>,
}

/// A live write key: what the ingest path needs to admit a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    pub key: String,
    pub site_id: u32,
    pub origins: Vec<String>,
}

/// The active schema versions of one site, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct SiteSchemas {
    pub site_id: u32,
    pub versions: Vec<(u32, Schema)>,
}

/// Everything the ingest path caches, read in one transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub generation: u64,
    pub keys: Vec<KeyEntry>,
    pub schemas: Vec<SiteSchemas>,
    /// Operational settings stored by an operator, by name. The meaning of a
    /// name and its range belong to whoever reads it.
    pub settings: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum MetaError {
    /// The named thing does not exist.
    NotFound(&'static str),
    /// An argument is not acceptable; the message says which and why.
    Invalid(String),
    /// The change clashes with what exists (a name in use, a version already
    /// retired, the last active version).
    Conflict(String),
    /// The schema file does not parse or breaks a rule.
    InvalidSchema(Vec<Diagnostic>),
    /// The schema changes something older apps depend on.
    Breaking(Vec<Change>),
    /// The schema declares the same thing as the latest active version.
    Unchanged,
    /// A stored value no longer reads back; the store was altered outside
    /// Dionaea.
    Corrupt(String),
    /// The database failed.
    Storage(String),
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(what) => write!(f, "{what} not found"),
            Self::Invalid(message) => write!(f, "invalid: {message}"),
            Self::Conflict(message) => write!(f, "conflict: {message}"),
            Self::InvalidSchema(diagnostics) => {
                write!(f, "the schema is invalid ({} problems)", diagnostics.len())
            }
            Self::Breaking(changes) => {
                write!(f, "the schema has {} breaking changes", changes.len())
            }
            Self::Unchanged => f.write_str("the schema is the same as the latest version"),
            Self::Corrupt(message) => write!(f, "stored data is corrupt: {message}"),
            Self::Storage(message) => write!(f, "metadata storage failed: {message}"),
        }
    }
}

impl std::error::Error for MetaError {}

/// Where metadata lives. Every method is synchronous; callers on an async
/// runtime run them on a blocking thread.
pub trait MetaStore: Send + Sync {
    /// Checks the store answers.
    fn ping(&self) -> Result<(), MetaError>;

    /// A number that increases with every change a [`Snapshot`] can show.
    fn generation(&self) -> Result<u64, MetaError>;

    fn create_organization(&self, name: &str) -> Result<Organization, MetaError>;

    fn organizations(&self) -> Result<Vec<Organization>, MetaError>;

    fn create_site(&self, organization_id: u32, name: &str) -> Result<Site, MetaError>;

    fn sites(&self) -> Result<Vec<Site>, MetaError>;

    /// Issues a write key for a site. `origins` are the web origins allowed
    /// to use it (`https://example.com`, no path); empty for a native app.
    fn issue_write_key(
        &self,
        site_id: u32,
        label: &str,
        origins: &[String],
    ) -> Result<WriteKeyInfo, MetaError>;

    /// Revokes a key. Revoking a revoked key is not an error.
    fn revoke_write_key(&self, key: &str) -> Result<(), MetaError>;

    fn write_keys(&self, site_id: u32) -> Result<Vec<WriteKeyInfo>, MetaError>;

    /// What publishing `source` would change against the latest active
    /// version of the site, without storing anything.
    fn diff_schema(&self, site_id: u32, source: &str) -> Result<Vec<Change>, MetaError>;

    /// Publishes a schema file as the next version of the site. A published
    /// version is immutable. Refused when the file is invalid, breaks
    /// compatibility with the latest active version, or changes nothing.
    fn publish_schema(&self, site_id: u32, source: &str) -> Result<Published, MetaError>;

    /// Stops accepting events under a version. The last active version of a
    /// site cannot be retired.
    fn retire_schema_version(&self, site_id: u32, version: u32) -> Result<(), MetaError>;

    fn schema_versions(&self, site_id: u32) -> Result<Vec<SchemaVersionInfo>, MetaError>;

    /// The operational settings an operator stored, by name.
    fn settings(&self) -> Result<BTreeMap<String, String>, MetaError>;

    /// Stores a setting, replacing its value. Only the shape of the name
    /// (lowercase letters, digits and `_`) and the value (printable, not
    /// empty) is checked here. Changing the stored value moves the generation.
    fn set_setting(&self, name: &str, value: &str) -> Result<(), MetaError>;

    /// Removes a stored setting. Returns whether it existed.
    fn unset_setting(&self, name: &str) -> Result<bool, MetaError>;

    /// The live keys and active schema versions of every site.
    fn snapshot(&self) -> Result<Snapshot, MetaError>;
}

/// Whether `key` has the shape of a write key. The ingest path uses it to
/// reject garbage before looking anything up.
pub fn is_write_key_shape(key: &str) -> bool {
    key.strip_prefix(sqlite::KEY_PREFIX).is_some_and(|rest| {
        rest.len() == sqlite::KEY_CHARS
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    })
}
