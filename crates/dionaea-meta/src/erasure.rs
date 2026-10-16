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
//! The hash is HMAC-SHA-256, keyed with a secret the operator configures
//! (the same on every node), over a fixed prefix, the kind, the site and the
//! identifier. Someone who holds only the metadata file cannot test whether a
//! guessable identifier (`u_42`) was erased. The key is never stored; a check
//! value derived from it is, so a node started with another key refuses to
//! run instead of silently missing every tombstone. There is no rotation:
//! the identifiers are gone, so the hashes cannot be recomputed.

use std::collections::HashSet;
use std::fmt;
use std::sync::{PoisonError, RwLock, RwLockReadGuard};

use sha2::{Digest, Sha256};

use crate::{MetaError, MetaStore};

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

const BLOCK: usize = 64;

/// HMAC-SHA-256 (RFC 2104) of the concatenation of `parts`.
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        text.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    text
}

/// The secret that keys the tombstone hashes.
#[derive(Clone, PartialEq, Eq)]
pub struct HashKey([u8; 32]);

impl fmt::Debug for HashKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashKey(..)")
    }
}

/// Shortest and longest secret accepted, in bytes.
pub const MIN_KEY_LEN: usize = 32;
pub const MAX_KEY_LEN: usize = 512;

impl HashKey {
    /// A key from the operator's secret: 32 to 512 bytes of printable text,
    /// random enough that it cannot be guessed (`openssl rand -hex 32`).
    ///
    /// # Errors
    ///
    /// The reason the secret is not acceptable.
    pub fn from_secret(secret: &str) -> Result<Self, String> {
        if !(MIN_KEY_LEN..=MAX_KEY_LEN).contains(&secret.len()) {
            return Err(format!(
                "the erasure key is {MIN_KEY_LEN} to {MAX_KEY_LEN} characters"
            ));
        }
        if secret.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err("the erasure key has no spaces or control characters".to_owned());
        }
        let mut hasher = Sha256::new();
        hasher.update(b"dionaea/erasure/key/v1\0");
        hasher.update(secret.as_bytes());
        Ok(Self(hasher.finalize().into()))
    }

    fn digest(&self, site_id: u32, kind: IdKind, id: &str) -> [u8; 32] {
        hmac_sha256(
            &self.0,
            &[
                b"dionaea/erasure/v1\0",
                kind.as_str().as_bytes(),
                &[0],
                &site_id.to_le_bytes(),
                id.as_bytes(),
            ],
        )
    }

    /// The value stored to recognise this key later, never the key.
    pub(crate) fn check(&self) -> String {
        hex(&hmac_sha256(&self.0, &[b"dionaea/erasure/key-check"]))
    }

    /// The tombstone hash of an identifier, in lowercase hex.
    pub fn tombstone_hash(&self, site_id: u32, kind: IdKind, id: &str) -> String {
        hex(&self.digest(site_id, kind, id))
    }

    fn short_of(&self, site_id: u32, kind: IdKind, id: &str) -> u128 {
        let digest = self.digest(site_id, kind, id);
        let mut head = [0u8; 16];
        head.copy_from_slice(&digest[..16]);
        u128::from_be_bytes(head)
    }
}

/// The first 128 bits of a stored hash: what a node keeps in memory. Two
/// different identifiers sharing them is as unlikely as a collision of the
/// whole hash would be for a set of any size that fits in memory.
fn short(hex: &str) -> Option<u128> {
    u128::from_str_radix(hex.get(..32)?, 16).ok()
}

/// New tombstones, in the order they were made, and where to ask from next.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TombstoneBatch {
    /// The position of the last entry (or the position asked from when there
    /// are none).
    pub last: u64,
    /// Site, kind and stored hash of each.
    pub entries: Vec<(u32, IdKind, String)>,
}

#[derive(Debug, Default)]
struct Inner {
    users: HashSet<(u32, u128)>,
    anons: HashSet<(u32, u128)>,
    last: u64,
    /// The key the hashes were made with, once the store has said which.
    key: Option<HashKey>,
}

/// Every tombstone, for the ingest path to consult without a query. They are
/// never removed, so a node learns of new ones by asking only for what came
/// after the last it saw, and keeps 128 bits of each hash.
#[derive(Debug, Default)]
pub struct Tombstones {
    inner: RwLock<Inner>,
}

/// How many tombstones one read of the store returns.
const BATCH: usize = 10_000;

impl Tombstones {
    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds what the store has made since the last call. Returns how many
    /// were new.
    ///
    /// # Errors
    ///
    /// When the store cannot be read; what was added before stays.
    pub fn load_from(&self, store: &dyn MetaStore) -> Result<usize, MetaError> {
        let mut added = 0;
        self.inner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .key = store.erasure_key();
        loop {
            let after = self.read().last;
            let batch = store.tombstones_since(after, BATCH)?;
            if batch.entries.is_empty() {
                return Ok(added);
            }
            let full = batch.entries.len() >= BATCH;
            let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
            for (site, kind, hash) in &batch.entries {
                let Some(short) = short(hash) else {
                    return Err(MetaError::Corrupt("a tombstone is not a hash".to_owned()));
                };
                let fresh = match kind {
                    IdKind::User => inner.users.insert((*site, short)),
                    IdKind::Anon => inner.anons.insert((*site, short)),
                };
                added += usize::from(fresh);
            }
            inner.last = inner.last.max(batch.last);
            drop(inner);
            if !full {
                return Ok(added);
            }
        }
    }

    /// The position of the last tombstone this holds (0 when none).
    pub fn last_seen(&self) -> u64 {
        self.read().last
    }

    pub fn is_empty(&self) -> bool {
        let inner = self.read();
        inner.users.is_empty() && inner.anons.is_empty()
    }

    pub fn len(&self) -> usize {
        let inner = self.read();
        inner.users.len() + inner.anons.len()
    }

    /// Whether this user of the site was erased or is being erased.
    pub fn user_erased(&self, site_id: u32, user_id: &str) -> bool {
        let inner = self.read();
        let Some(key) = &inner.key else {
            return false;
        };
        !inner.users.is_empty()
            && inner
                .users
                .contains(&(site_id, key.short_of(site_id, IdKind::User, user_id)))
    }

    /// Whether this device of the site belonged to an erased user.
    pub fn anon_erased(&self, site_id: u32, anon_id: &str) -> bool {
        let inner = self.read();
        let Some(key) = &inner.key else {
            return false;
        };
        !inner.anons.is_empty()
            && inner
                .anons
                .contains(&(site_id, key.short_of(site_id, IdKind::Anon, anon_id)))
    }
}
