//! Erasure requests and the tombstones that keep erased data from coming
//! back.
//!
//! A request names a user of a site. The ingest path must stop accepting that
//! user's data the moment the request exists, and later events from the
//! devices that were linked to the user, so the request leaves tombstones:
//! one for the user id and, once the job has resolved them, one for each
//! anonymous id. A tombstone is only a hash, never the identifier, and lives
//! as long as the metadata store does.
//!
//! The hash is SHA-256 over a fixed prefix, the kind, the site and the
//! identifier. It is not salted: a node must be able to recompute it for
//! every request, and a salt would be one more secret to keep. The cost is
//! that someone who holds the metadata file can test whether a guessable
//! identifier (`u_42`) was erased.

use std::collections::HashSet;
use std::fmt;

use sha2::{Digest, Sha256};

/// Where a request is. The job moves it forward one step at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureState {
    /// Recorded; nothing deleted yet.
    Requested,
    /// The user's rows are deleted (invisible to every query); the physical
    /// removal is still to come.
    Hidden,
    /// The rows are physically gone and a count confirmed it.
    Complete,
    /// The job gave up; `detail` says why. An operator may retry.
    Failed,
}

impl ErasureState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Hidden => "hidden",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "requested" => Some(Self::Requested),
            "hidden" => Some(Self::Hidden),
            "complete" => Some(Self::Complete),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

impl fmt::Display for ErasureState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An erasure request as listed for administration and the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureRequest {
    pub id: u64,
    pub site_id: u32,
    /// The user id, until the request is complete; then `None`, and only the
    /// hash remains.
    pub user_id: Option<String>,
    /// The tombstone hash of the user id: the audit record of who was erased.
    pub user_hash: String,
    pub state: ErasureState,
    pub requested_by: String,
    /// Seconds since the Unix epoch.
    pub requested_at: i64,
    pub hidden_at: Option<i64>,
    pub completed_at: Option<i64>,
    /// How many anonymous ids were linked to the user and tombstoned.
    pub anon_count: u64,
    /// What the job reports: the rows removed per table, or why it failed.
    pub detail: Option<String>,
    /// How many times trouble that retrying may fix has stopped this request
    /// in its current state.
    pub attempts: u32,
    /// Seconds since the epoch before which it is not tried again.
    pub retry_after: Option<i64>,
}

/// How many requests are where, for the metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ErasureSummary {
    pub requested: u64,
    pub hidden: u64,
    pub complete: u64,
    pub failed: u64,
    /// Requests waiting to be tried again after trouble.
    pub retrying: u64,
    /// When the oldest unfinished request was asked for, in seconds.
    pub oldest_open_at: Option<i64>,
}

/// What an identifier is, for tombstones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    User,
    Anon,
}

impl IdKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Anon => "anon",
        }
    }
}

/// The tombstone hash of an identifier, in lowercase hex.
pub fn tombstone_hash(site_id: u32, kind: IdKind, id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dionaea/erasure/v1\0");
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(site_id.to_le_bytes());
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        hex.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    hex
}

/// Every tombstone, for the ingest path to consult without a query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tombstones {
    users: HashSet<(u32, String)>,
    anons: HashSet<(u32, String)>,
}

impl Tombstones {
    pub(crate) fn insert(&mut self, site_id: u32, kind: IdKind, hash: String) {
        match kind {
            IdKind::User => self.users.insert((site_id, hash)),
            IdKind::Anon => self.anons.insert((site_id, hash)),
        };
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && self.anons.is_empty()
    }

    pub fn len(&self) -> usize {
        self.users.len() + self.anons.len()
    }

    /// Whether this user of the site was erased or is being erased.
    pub fn user_erased(&self, site_id: u32, user_id: &str) -> bool {
        !self.users.is_empty()
            && self
                .users
                .contains(&(site_id, tombstone_hash(site_id, IdKind::User, user_id)))
    }

    /// Whether this device of the site belonged to an erased user.
    pub fn anon_erased(&self, site_id: u32, anon_id: &str) -> bool {
        !self.anons.is_empty()
            && self
                .anons
                .contains(&(site_id, tombstone_hash(site_id, IdKind::Anon, anon_id)))
    }
}
