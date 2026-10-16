//! The in-memory copy of the keys and schemas the ingest path needs, so a
//! request never queries the metadata store. [`Cache::refresh`] compares one
//! number with the store and reloads only when it changed.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use dionaea_core::Schema;
use dionaea_meta::{MetaError, MetaStore};

/// Why a write key was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The key does not exist or was revoked.
    UnknownKey,
    /// The key lists origins and the request's is not one of them.
    OriginNotAllowed,
}

impl Refusal {
    /// The name used for counters.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownKey => "unknown_key",
            Self::OriginNotAllowed => "origin_not_allowed",
        }
    }
}

/// What a request that passed admission may write to: a site and the schema
/// versions that are still accepted, newest first.
#[derive(Debug, Clone)]
pub struct Admission {
    pub site_id: u32,
    pub schemas: Arc<[(u32, Schema)]>,
}

#[derive(Debug)]
struct KeyEntry {
    site_id: u32,
    /// Lowercase; empty means no origin check (native apps).
    origins: Vec<String>,
}

#[derive(Debug, Default)]
struct Table {
    /// Whether this came from the store; an empty cache has not.
    loaded: bool,
    generation: u64,
    keys: HashMap<String, KeyEntry>,
    schemas: HashMap<u32, Arc<[(u32, Schema)]>>,
}

/// Keys and schemas, shared by every request.
#[derive(Debug, Default)]
pub struct Cache {
    table: RwLock<Arc<Table>>,
}

impl Cache {
    /// An empty cache: every key is unknown until [`refresh`](Self::refresh).
    pub fn new() -> Self {
        Self::default()
    }

    fn current(&self) -> Arc<Table> {
        Arc::clone(&self.table.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The generation of the data the cache holds.
    pub fn generation(&self) -> u64 {
        self.current().generation
    }

    /// Reloads from the store when its generation differs from the one held.
    /// Returns whether it reloaded. On error the previous data stays in use.
    pub fn refresh(&self, store: &dyn MetaStore) -> Result<bool, MetaError> {
        let held = self.current();
        let latest = store.generation()?;
        if held.loaded && latest == held.generation {
            return Ok(false);
        }
        let snapshot = store.snapshot()?;
        let mut table = Table {
            loaded: true,
            generation: snapshot.generation,
            keys: HashMap::with_capacity(snapshot.keys.len()),
            schemas: HashMap::with_capacity(snapshot.schemas.len()),
        };
        for key in snapshot.keys {
            table.keys.insert(
                key.key,
                KeyEntry {
                    site_id: key.site_id,
                    origins: key
                        .origins
                        .into_iter()
                        .map(|o| o.to_ascii_lowercase())
                        .collect(),
                },
            );
        }
        for site in snapshot.schemas {
            // Newest first: most events are valid under the latest version.
            let mut versions = site.versions;
            versions.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
            table.schemas.insert(site.site_id, versions.into());
        }
        Ok(self.install(table))
    }

    /// Replaces the held data unless it is already newer: two refreshes that
    /// overlap must not let the older snapshot win, or a revoked key would
    /// come back until the next change.
    fn install(&self, table: Table) -> bool {
        let mut held = self.table.write().unwrap_or_else(PoisonError::into_inner);
        if held.loaded && held.generation > table.generation {
            return false;
        }
        *held = Arc::new(table);
        true
    }

    /// Resolves a write key and checks the request's `Origin` header.
    pub fn admit(&self, key: &str, origin: Option<&str>) -> Result<Admission, Refusal> {
        let table = self.current();
        let entry = table.keys.get(key).ok_or(Refusal::UnknownKey)?;
        if !entry.origins.is_empty() {
            let allowed = origin.is_some_and(|o| {
                entry
                    .origins
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(o.trim()))
            });
            if !allowed {
                return Err(Refusal::OriginNotAllowed);
            }
        }
        let schemas = table
            .schemas
            .get(&entry.site_id)
            .cloned()
            .unwrap_or_else(|| Arc::from([]));
        Ok(Admission {
            site_id: entry.site_id,
            schemas,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(generation: u64, key: &str) -> Table {
        let mut table = Table {
            loaded: true,
            generation,
            ..Table::default()
        };
        table.keys.insert(
            key.to_owned(),
            KeyEntry {
                site_id: 1,
                origins: Vec::new(),
            },
        );
        table
    }

    #[test]
    fn an_older_snapshot_never_replaces_a_newer_one() {
        let cache = Cache::new();
        assert!(cache.install(table(6, "new")));
        assert!(
            !cache.install(table(5, "old")),
            "an overlapping refresh lost the race"
        );
        assert_eq!(cache.generation(), 6);
        assert!(cache.admit("new", None).is_ok());
        assert_eq!(cache.admit("old", None).unwrap_err(), Refusal::UnknownKey);
        assert!(cache.install(table(7, "newer")));
        assert!(
            cache.install(table(7, "same")),
            "the same generation is idempotent"
        );
    }
}
