//! The SQLite implementation: one connection behind a mutex, WAL, one writer.
//! Metadata changes rarely and is read through [`Snapshot`], so a single
//! connection is enough and keeps every operation easy to reason about.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dionaea_core::{Change, Schema, diff, is_breaking, parse_schema};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::migrations::MIGRATIONS;
use crate::{
    KeyEntry, MAX_LABEL_LEN, MAX_ORIGINS, MetaError, MetaStore, Organization, Published,
    SchemaVersionInfo, Site, SiteSchemas, Snapshot, WriteKeyInfo,
};

pub(crate) const KEY_PREFIX: &str = "wk_";
/// 16 random bytes in lowercase base32 without padding.
pub(crate) const KEY_CHARS: usize = 26;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WAL_ATTEMPTS: u32 = 200;

type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

pub struct SqliteMeta {
    connection: Mutex<Connection>,
    clock: Clock,
}

impl fmt::Debug for SqliteMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteMeta").finish_non_exhaustive()
    }
}

fn storage(error: rusqlite::Error) -> MetaError {
    MetaError::Storage(error.to_string())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl SqliteMeta {
    /// Opens (creating if needed) the database file and applies the pending
    /// migrations.
    pub fn open(path: &Path) -> Result<Self, MetaError> {
        let connection = Connection::open(path).map_err(storage)?;
        // Before anything that takes a lock: another process may be opening the
        // same new file.
        connection.busy_timeout(BUSY_TIMEOUT).map_err(storage)?;
        // Switching a new file to WAL can answer "locked" at once, without
        // waiting, while another process does the same; it is safe to repeat.
        let mut attempts = 0;
        while let Err(error) = connection.pragma_update(None, "journal_mode", "WAL") {
            attempts += 1;
            if attempts >= WAL_ATTEMPTS {
                return Err(storage(error));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Self::from_connection(connection)
    }

    /// A database that lives only as long as this value; for tests.
    pub fn open_in_memory() -> Result<Self, MetaError> {
        Self::from_connection(Connection::open_in_memory().map_err(storage)?)
    }

    fn from_connection(connection: Connection) -> Result<Self, MetaError> {
        connection.busy_timeout(BUSY_TIMEOUT).map_err(storage)?;
        // Metadata changes rarely; a committed change must survive power loss.
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(storage)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage)?;
        let mut store = Self {
            connection: Mutex::new(connection),
            clock: Box::new(now),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Replaces the clock, in seconds since the Unix epoch.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    fn migrate(&mut self) -> Result<(), MetaError> {
        let connection = self
            .connection
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS _migrations (
                     version INTEGER PRIMARY KEY,
                     name    TEXT NOT NULL
                 )",
            )
            .map_err(storage)?;
        for migration in MIGRATIONS {
            // IMMEDIATE takes the write lock first, so two processes starting
            // together apply a migration once: the second finds it recorded.
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let applied: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM _migrations WHERE version = ?1)",
                    [migration.version],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if applied {
                continue;
            }
            tx.execute_batch(migration.sql).map_err(storage)?;
            tx.execute(
                "INSERT INTO _migrations (version, name) VALUES (?1, ?2)",
                params![migration.version, migration.name],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
        }
        Ok(())
    }

    /// Runs `work` in a write transaction and commits it.
    fn write<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>, i64) -> Result<T, MetaError>,
    ) -> Result<T, MetaError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let value = work(&tx, (self.clock)())?;
        tx.commit().map_err(storage)?;
        Ok(value)
    }

    /// Runs `work` in a read transaction: everything it reads is one
    /// consistent state.
    fn read<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, MetaError>,
    ) -> Result<T, MetaError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let tx = connection.transaction().map_err(storage)?;
        work(&tx)
    }
}

fn bump(tx: &Transaction<'_>) -> Result<(), MetaError> {
    tx.execute("UPDATE state SET generation = generation + 1", [])
        .map_err(storage)?;
    Ok(())
}

fn generation_of(tx: &Transaction<'_>) -> Result<u64, MetaError> {
    let value: i64 = tx
        .query_row("SELECT generation FROM state", [], |row| row.get(0))
        .map_err(storage)?;
    u64::try_from(value).map_err(|_| MetaError::Corrupt("negative generation".to_owned()))
}

fn id_of(value: i64) -> Result<u32, MetaError> {
    u32::try_from(value).map_err(|_| MetaError::Corrupt(format!("id {value} is out of range")))
}

fn clean_label(value: &str, what: &str) -> Result<String, MetaError> {
    let value = value.trim();
    let length = value.chars().count();
    if length == 0 || length > MAX_LABEL_LEN {
        return Err(MetaError::Invalid(format!(
            "{what} must be 1 to {MAX_LABEL_LEN} characters"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(MetaError::Invalid(format!(
            "{what} must not contain control characters"
        )));
    }
    Ok(value.to_owned())
}

/// Normalizes an origin: `https://host[:port]`, lowercase, no path.
fn clean_origin(value: &str) -> Result<String, MetaError> {
    let bad = |why: &str| MetaError::Invalid(format!("origin `{value}`: {why}"));
    let origin = value.trim().to_ascii_lowercase();
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(|| bad("must start with http:// or https://"))?;
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'));
    if !host_ok {
        return Err(bad("the host is not a plain host name or address"));
    }
    if let Some(port) = port
        && !(!port.is_empty()
            && port.len() <= 5
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p)))
    {
        return Err(bad("the port is not a number from 1 to 65535"));
    }
    Ok(origin)
}

fn new_key() -> Result<String, MetaError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| MetaError::Storage(format!("no randomness available: {e}")))?;
    let mut key = String::with_capacity(KEY_PREFIX.len() + KEY_CHARS);
    key.push_str(KEY_PREFIX);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            key.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        key.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }
    Ok(key)
}

fn origins_of(tx: &Transaction<'_>, key: &str) -> Result<Vec<String>, MetaError> {
    let mut statement = tx
        .prepare_cached("SELECT origin FROM key_origins WHERE key = ?1 ORDER BY origin")
        .map_err(storage)?;
    statement
        .query_map([key], |row| row.get(0))
        .map_err(storage)?
        .collect::<Result<_, _>>()
        .map_err(storage)
}

fn site_exists(tx: &Transaction<'_>, site_id: u32) -> Result<(), MetaError> {
    let found: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sites WHERE id = ?1)",
            [site_id],
            |row| row.get(0),
        )
        .map_err(storage)?;
    found.then_some(()).ok_or(MetaError::NotFound("site"))
}

fn parse_stored(source: &str, site_id: u32, version: u32) -> Result<Schema, MetaError> {
    parse_schema(source).map_err(|_| {
        MetaError::Corrupt(format!(
            "version {version} of site {site_id} no longer parses"
        ))
    })
}

/// The latest active version of a site, if there is one.
fn latest_active(tx: &Transaction<'_>, site_id: u32) -> Result<Option<Schema>, MetaError> {
    let row: Option<(i64, String)> = tx
        .query_row(
            "SELECT version, source FROM schema_versions
             WHERE site_id = ?1 AND retired_at IS NULL ORDER BY version DESC LIMIT 1",
            [site_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage)?;
    row.map(|(version, source)| parse_stored(&source, site_id, id_of(version)?))
        .transpose()
}

fn parse_new(source: &str) -> Result<Schema, MetaError> {
    parse_schema(source).map_err(MetaError::InvalidSchema)
}

fn key_info(tx: &Transaction<'_>, row: &rusqlite::Row<'_>) -> Result<WriteKeyInfo, MetaError> {
    let key: String = row.get(0).map_err(storage)?;
    let origins = origins_of(tx, &key)?;
    Ok(WriteKeyInfo {
        site_id: id_of(row.get(1).map_err(storage)?)?,
        label: row.get(2).map_err(storage)?,
        created_at: row.get(3).map_err(storage)?,
        revoked_at: row.get(4).map_err(storage)?,
        origins,
        key,
    })
}

impl MetaStore for SqliteMeta {
    fn ping(&self) -> Result<(), MetaError> {
        self.read(|tx| tx.query_row("SELECT 1", [], |_| Ok(())).map_err(storage))
    }

    fn generation(&self) -> Result<u64, MetaError> {
        self.read(generation_of)
    }

    fn create_organization(&self, name: &str) -> Result<Organization, MetaError> {
        let name = clean_label(name, "an organization name")?;
        self.write(|tx, at| {
            let taken: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM organizations WHERE name = ?1)",
                    [&name],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if taken {
                return Err(MetaError::Conflict(format!(
                    "an organization named `{name}` exists"
                )));
            }
            tx.execute(
                "INSERT INTO organizations (name, created_at) VALUES (?1, ?2)",
                params![name, at],
            )
            .map_err(storage)?;
            Ok(Organization {
                id: id_of(tx.last_insert_rowid())?,
                name,
            })
        })
    }

    fn create_site(&self, organization_id: u32, name: &str) -> Result<Site, MetaError> {
        let name = clean_label(name, "a site name")?;
        self.write(|tx, at| {
            let found: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM organizations WHERE id = ?1)",
                    [organization_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if !found {
                return Err(MetaError::NotFound("organization"));
            }
            let taken: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sites WHERE org_id = ?1 AND name = ?2)",
                    params![organization_id, name],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if taken {
                return Err(MetaError::Conflict(format!("a site named `{name}` exists")));
            }
            tx.execute(
                "INSERT INTO sites (org_id, name, created_at) VALUES (?1, ?2, ?3)",
                params![organization_id, name, at],
            )
            .map_err(storage)?;
            Ok(Site {
                id: id_of(tx.last_insert_rowid())?,
                organization_id,
                name,
            })
        })
    }

    fn sites(&self) -> Result<Vec<Site>, MetaError> {
        self.read(|tx| {
            let mut statement = tx
                .prepare("SELECT id, org_id, name FROM sites ORDER BY id")
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get(2)?))
                })
                .map_err(storage)?;
            rows.map(|row| {
                let (id, organization_id, name) = row.map_err(storage)?;
                Ok(Site {
                    id: id_of(id)?,
                    organization_id: id_of(organization_id)?,
                    name,
                })
            })
            .collect()
        })
    }

    fn issue_write_key(
        &self,
        site_id: u32,
        label: &str,
        origins: &[String],
    ) -> Result<WriteKeyInfo, MetaError> {
        let label = clean_label(label, "a key label")?;
        if origins.len() > MAX_ORIGINS {
            return Err(MetaError::Invalid(format!(
                "a key allows at most {MAX_ORIGINS} origins"
            )));
        }
        let origins = origins
            .iter()
            .map(|origin| clean_origin(origin))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        let key = new_key()?;
        self.write(|tx, at| {
            site_exists(tx, site_id)?;
            tx.execute(
                "INSERT INTO write_keys (key, site_id, label, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![key, site_id, label, at],
            )
            .map_err(storage)?;
            for origin in &origins {
                tx.execute(
                    "INSERT INTO key_origins (key, origin) VALUES (?1, ?2)",
                    params![key, origin],
                )
                .map_err(storage)?;
            }
            bump(tx)?;
            Ok(WriteKeyInfo {
                key,
                site_id,
                label,
                origins: origins.into_iter().collect(),
                created_at: at,
                revoked_at: None,
            })
        })
    }

    fn revoke_write_key(&self, key: &str) -> Result<(), MetaError> {
        self.write(|tx, at| {
            let state: Option<Option<i64>> = tx
                .query_row(
                    "SELECT revoked_at FROM write_keys WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            match state {
                None => Err(MetaError::NotFound("write key")),
                Some(Some(_)) => Ok(()),
                Some(None) => {
                    tx.execute(
                        "UPDATE write_keys SET revoked_at = ?2 WHERE key = ?1",
                        params![key, at],
                    )
                    .map_err(storage)?;
                    bump(tx)
                }
            }
        })
    }

    fn write_keys(&self, site_id: u32) -> Result<Vec<WriteKeyInfo>, MetaError> {
        self.read(|tx| {
            site_exists(tx, site_id)?;
            let mut statement = tx
                .prepare(
                    "SELECT key, site_id, label, created_at, revoked_at
                     FROM write_keys WHERE site_id = ?1 ORDER BY created_at, rowid",
                )
                .map_err(storage)?;
            let mut rows = statement.query([site_id]).map_err(storage)?;
            let mut keys = Vec::new();
            while let Some(row) = rows.next().map_err(storage)? {
                keys.push(key_info(tx, row)?);
            }
            Ok(keys)
        })
    }

    fn diff_schema(&self, site_id: u32, source: &str) -> Result<Vec<Change>, MetaError> {
        let new = parse_new(source)?;
        self.read(|tx| {
            site_exists(tx, site_id)?;
            Ok(match latest_active(tx, site_id)? {
                Some(old) => diff(&old, &new),
                None => diff(&Schema::default(), &new),
            })
        })
    }

    fn publish_schema(&self, site_id: u32, source: &str) -> Result<Published, MetaError> {
        let new = parse_new(source)?;
        self.write(|tx, at| {
            site_exists(tx, site_id)?;
            let changes = match latest_active(tx, site_id)? {
                Some(old) => {
                    let changes = diff(&old, &new);
                    if changes.is_empty() {
                        return Err(MetaError::Unchanged);
                    }
                    if is_breaking(&changes) {
                        return Err(MetaError::Breaking(changes));
                    }
                    changes
                }
                None => Vec::new(),
            };
            let last: Option<i64> = tx
                .query_row(
                    "SELECT MAX(version) FROM schema_versions WHERE site_id = ?1",
                    [site_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            let version = id_of(last.unwrap_or(0))?.checked_add(1).ok_or_else(|| {
                MetaError::Conflict("no more versions can be published".to_owned())
            })?;
            tx.execute(
                "INSERT INTO schema_versions (site_id, version, source, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![site_id, version, source, at],
            )
            .map_err(storage)?;
            bump(tx)?;
            Ok(Published { version, changes })
        })
    }

    fn retire_schema_version(&self, site_id: u32, version: u32) -> Result<(), MetaError> {
        self.write(|tx, at| {
            let state: Option<Option<i64>> = tx
                .query_row(
                    "SELECT retired_at FROM schema_versions WHERE site_id = ?1 AND version = ?2",
                    params![site_id, version],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            match state {
                None => return Err(MetaError::NotFound("schema version")),
                Some(Some(_)) => {
                    return Err(MetaError::Conflict(format!(
                        "version {version} is already retired"
                    )));
                }
                Some(None) => {}
            }
            let active: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM schema_versions
                     WHERE site_id = ?1 AND retired_at IS NULL",
                    [site_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if active <= 1 {
                return Err(MetaError::Conflict(
                    "the last active version of a site cannot be retired".to_owned(),
                ));
            }
            tx.execute(
                "UPDATE schema_versions SET retired_at = ?3 WHERE site_id = ?1 AND version = ?2",
                params![site_id, version, at],
            )
            .map_err(storage)?;
            bump(tx)
        })
    }

    fn schema_versions(&self, site_id: u32) -> Result<Vec<SchemaVersionInfo>, MetaError> {
        self.read(|tx| {
            site_exists(tx, site_id)?;
            let mut statement = tx
                .prepare(
                    "SELECT version, created_at, retired_at FROM schema_versions
                     WHERE site_id = ?1 ORDER BY version",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map([site_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                })
                .map_err(storage)?;
            rows.map(|row| {
                let (version, created_at, retired_at) = row.map_err(storage)?;
                Ok(SchemaVersionInfo {
                    site_id,
                    version: id_of(version)?,
                    created_at,
                    retired_at,
                })
            })
            .collect()
        })
    }

    fn snapshot(&self) -> Result<Snapshot, MetaError> {
        self.read(|tx| {
            let generation = generation_of(tx)?;

            let mut origins: BTreeMap<String, Vec<String>> = BTreeMap::new();
            {
                let mut statement = tx
                    .prepare(
                        "SELECT o.key, o.origin FROM key_origins o
                         JOIN write_keys k ON k.key = o.key
                         WHERE k.revoked_at IS NULL ORDER BY o.key, o.origin",
                    )
                    .map_err(storage)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(storage)?;
                for row in rows {
                    let (key, origin) = row.map_err(storage)?;
                    origins.entry(key).or_default().push(origin);
                }
            }
            let mut keys = Vec::new();
            {
                let mut statement = tx
                    .prepare(
                        "SELECT key, site_id FROM write_keys WHERE revoked_at IS NULL ORDER BY key",
                    )
                    .map_err(storage)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })
                    .map_err(storage)?;
                for row in rows {
                    let (key, site_id) = row.map_err(storage)?;
                    keys.push(KeyEntry {
                        origins: origins.remove(&key).unwrap_or_default(),
                        site_id: id_of(site_id)?,
                        key,
                    });
                }
            }

            let mut schemas: Vec<SiteSchemas> = Vec::new();
            {
                let mut statement = tx
                    .prepare(
                        "SELECT site_id, version, source FROM schema_versions
                         WHERE retired_at IS NULL ORDER BY site_id, version",
                    )
                    .map_err(storage)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(storage)?;
                for row in rows {
                    let (site_id, version, source) = row.map_err(storage)?;
                    let (site_id, version) = (id_of(site_id)?, id_of(version)?);
                    let schema = parse_stored(&source, site_id, version)?;
                    match schemas.last_mut() {
                        Some(last) if last.site_id == site_id => {
                            last.versions.push((version, schema));
                        }
                        _ => schemas.push(SiteSchemas {
                            site_id,
                            versions: vec![(version, schema)],
                        }),
                    }
                }
            }

            Ok(Snapshot {
                generation,
                keys,
                schemas,
            })
        })
    }
}
