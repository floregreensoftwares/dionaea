//! The SQL of erasure requests and tombstones, as functions over a
//! transaction. `SqliteMeta` wraps each in its own transaction.

use dionaea_core::is_id;
use rusqlite::{OptionalExtension, Row, Transaction, params};

use crate::erasure::{
    ErasureRequest, ErasureState, ErasureSummary, HashKey, IdKind, TombstoneBatch,
};
use crate::sqlite::{bump, id_of, storage};
use crate::{MAX_LABEL_LEN, MetaError};

const COLUMNS: &str = "id, site_id, user_id, user_hash, state, requested_by, requested_at,
    hidden_at, completed_at, anon_count, detail, attempts, retry_after";

fn request_of(row: &Row<'_>) -> rusqlite::Result<(i64, i64, ErasureRequestRest)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        ErasureRequestRest {
            user_id: row.get(2)?,
            user_hash: row.get(3)?,
            state: row.get(4)?,
            requested_by: row.get(5)?,
            requested_at: row.get(6)?,
            hidden_at: row.get(7)?,
            completed_at: row.get(8)?,
            anon_count: row.get(9)?,
            detail: row.get(10)?,
            attempts: row.get(11)?,
            retry_after: row.get(12)?,
        },
    ))
}

struct ErasureRequestRest {
    user_id: Option<String>,
    user_hash: String,
    state: String,
    requested_by: String,
    requested_at: i64,
    hidden_at: Option<i64>,
    completed_at: Option<i64>,
    anon_count: i64,
    detail: Option<String>,
    attempts: i64,
    retry_after: Option<i64>,
}

fn build(parts: (i64, i64, ErasureRequestRest)) -> Result<ErasureRequest, MetaError> {
    let (id, site_id, rest) = parts;
    Ok(ErasureRequest {
        id: u64::try_from(id).map_err(|_| MetaError::Corrupt("negative request id".to_owned()))?,
        site_id: id_of(site_id)?,
        user_id: rest.user_id,
        user_hash: rest.user_hash,
        state: ErasureState::parse(&rest.state)
            .ok_or_else(|| MetaError::Corrupt(format!("unknown erasure state `{}`", rest.state)))?,
        requested_by: rest.requested_by,
        requested_at: rest.requested_at,
        hidden_at: rest.hidden_at,
        completed_at: rest.completed_at,
        anon_count: u64::try_from(rest.anon_count).unwrap_or(0),
        detail: rest.detail,
        attempts: u32::try_from(rest.attempts).unwrap_or(u32::MAX),
        retry_after: rest.retry_after,
    })
}

pub(crate) fn get(tx: &Transaction<'_>, id: u64) -> Result<ErasureRequest, MetaError> {
    let parts = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM erasure_requests WHERE id = ?1"),
            [i64::try_from(id).unwrap_or(i64::MAX)],
            request_of,
        )
        .optional()
        .map_err(storage)?
        .ok_or(MetaError::NotFound("erasure request"))?;
    build(parts)
}

pub(crate) fn list(
    tx: &Transaction<'_>,
    state: Option<ErasureState>,
) -> Result<Vec<ErasureRequest>, MetaError> {
    let mut statement = tx
        .prepare(&format!(
            "SELECT {COLUMNS} FROM erasure_requests
             WHERE (?1 IS NULL OR state = ?1) ORDER BY id"
        ))
        .map_err(storage)?;
    let rows = statement
        .query_map([state.map(ErasureState::as_str)], request_of)
        .map_err(storage)?;
    rows.map(|row| build(row.map_err(storage)?)).collect()
}

fn tombstone(
    tx: &Transaction<'_>,
    site_id: u32,
    kind: IdKind,
    hash: &str,
    request: u64,
    at: i64,
) -> Result<bool, MetaError> {
    let added = tx
        .execute(
            "INSERT OR IGNORE INTO erasure_tombstones (site_id, kind, hash, request_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                site_id,
                kind.as_str(),
                hash,
                i64::try_from(request).unwrap_or(i64::MAX),
                at
            ],
        )
        .map_err(storage)?;
    Ok(added > 0)
}

/// The key to hash with: the one configured, checked against the value
/// stored the first time one was used. Without a key nothing is tombstoned.
pub(crate) fn use_key<'a>(
    tx: &Transaction<'_>,
    key: Option<&'a HashKey>,
) -> Result<&'a HashKey, MetaError> {
    let key = key.ok_or_else(|| {
        MetaError::Invalid(
            "no erasure key is configured; set `erasure_key` (or `erasure_key_file`) \
             to a random secret of 32 or more characters, the same on every server"
                .to_owned(),
        )
    })?;
    match stored_check(tx)? {
        Some(stored) if stored != key.check() => Err(wrong_key()),
        Some(_) => Ok(key),
        None => {
            tx.execute("UPDATE state SET erasure_key_check = ?1", [key.check()])
                .map_err(storage)?;
            Ok(key)
        }
    }
}

fn wrong_key() -> MetaError {
    MetaError::Invalid(
        "the erasure key is not the one earlier erasures were made with; erasures cannot be \
         checked or made until the original key is configured again"
            .to_owned(),
    )
}

fn stored_check(tx: &Transaction<'_>) -> Result<Option<String>, MetaError> {
    tx.query_row("SELECT erasure_key_check FROM state", [], |row| row.get(0))
        .map_err(storage)
}

/// Before tombstones are read: if there are any, the configured key must be
/// the one they were made with.
pub(crate) fn check_key_for_reading(
    tx: &Transaction<'_>,
    key: Option<&HashKey>,
) -> Result<(), MetaError> {
    let any: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM erasure_tombstones)",
            [],
            |row| row.get(0),
        )
        .map_err(storage)?;
    if !any {
        return Ok(());
    }
    match (key, stored_check(tx)?) {
        (Some(key), Some(stored)) if stored == key.check() => Ok(()),
        (None, _) => Err(MetaError::Invalid(
            "erasures exist but no erasure key is configured; set `erasure_key` (or \
             `erasure_key_file`) to the key they were made with"
                .to_owned(),
        )),
        _ => Err(wrong_key()),
    }
}

/// Records a request for a user of a site, and the tombstone that stops new
/// data of that user at once. A request still unfinished for the same user is
/// returned instead of a second one.
pub(crate) fn request(
    tx: &Transaction<'_>,
    key: Option<&HashKey>,
    site_id: u32,
    user_id: &str,
    requested_by: &str,
    at: i64,
) -> Result<ErasureRequest, MetaError> {
    if !is_id(user_id) {
        return Err(MetaError::Invalid(
            "a user id is 1 to 64 letters, digits and `_ . : -`".to_owned(),
        ));
    }
    let by = requested_by.trim();
    if by.is_empty() || by.chars().count() > MAX_LABEL_LEN || by.chars().any(char::is_control) {
        return Err(MetaError::Invalid(format!(
            "who requested it is 1 to {MAX_LABEL_LEN} printable characters"
        )));
    }
    let site: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sites WHERE id = ?1)",
            [site_id],
            |row| row.get(0),
        )
        .map_err(storage)?;
    if !site {
        return Err(MetaError::NotFound("site"));
    }
    let key = use_key(tx, key)?;
    let hash = key.tombstone_hash(site_id, IdKind::User, user_id);
    let open: Option<i64> = tx
        .query_row(
            "SELECT id FROM erasure_requests
             WHERE site_id = ?1 AND user_hash = ?2 AND state IN ('requested', 'hidden')",
            params![site_id, hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?;
    if let Some(id) = open {
        return get(tx, u64::try_from(id).unwrap_or(0));
    }
    tx.execute(
        "INSERT INTO erasure_requests (site_id, user_id, user_hash, state, requested_by, requested_at)
         VALUES (?1, ?2, ?3, 'requested', ?4, ?5)",
        params![site_id, user_id, hash, by, at],
    )
    .map_err(storage)?;
    let id = u64::try_from(tx.last_insert_rowid())
        .map_err(|_| MetaError::Corrupt("negative request id".to_owned()))?;
    tombstone(tx, site_id, IdKind::User, &hash, id, at)?;
    bump(tx)?;
    get(tx, id)
}

/// Takes the oldest request in `state` that nobody holds and that has been in
/// that state since `due_before` or earlier, for `lease_secs`.
pub(crate) fn claim(
    tx: &Transaction<'_>,
    state: ErasureState,
    worker: &str,
    lease_secs: i64,
    due_before: i64,
    at: i64,
) -> Result<Option<ErasureRequest>, MetaError> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM erasure_requests
             WHERE state = ?1 AND (lease_until IS NULL OR lease_until <= ?2)
               AND (retry_after IS NULL OR retry_after <= ?2)
               AND COALESCE(hidden_at, requested_at) <= ?3
             ORDER BY id LIMIT 1",
            params![state.as_str(), at, due_before],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?;
    let Some(id) = id else {
        return Ok(None);
    };
    tx.execute(
        "UPDATE erasure_requests SET claimed_by = ?2, lease_until = ?3 WHERE id = ?1",
        params![id, worker, at.saturating_add(lease_secs.max(1))],
    )
    .map_err(storage)?;
    get(tx, u64::try_from(id).unwrap_or(0)).map(Some)
}

/// Tombstones the anonymous ids linked to the user. Returns how many were new.
pub(crate) fn add_anons(
    tx: &Transaction<'_>,
    key: Option<&HashKey>,
    id: u64,
    anon_ids: &[String],
    at: i64,
) -> Result<u64, MetaError> {
    let request = get(tx, id)?;
    if request.state != ErasureState::Requested {
        return Err(MetaError::Conflict(
            "anonymous ids are added while a request is still `requested`".to_owned(),
        ));
    }
    let key = use_key(tx, key)?;
    let mut added = 0;
    for anon in anon_ids {
        if !is_id(anon) {
            // Not a shape the ingest path would have accepted, so nothing to
            // block and nothing to delete under it.
            continue;
        }
        let hash = key.tombstone_hash(request.site_id, IdKind::Anon, anon);
        tx.execute(
            "INSERT OR IGNORE INTO erasure_anons (request_id, anon_id) VALUES (?1, ?2)",
            params![i64::try_from(id).unwrap_or(i64::MAX), anon],
        )
        .map_err(storage)?;
        if tombstone(tx, request.site_id, IdKind::Anon, &hash, id, at)? {
            added += 1;
        }
    }
    if added > 0 {
        tx.execute(
            "UPDATE erasure_requests SET anon_count = anon_count + ?2 WHERE id = ?1",
            params![
                i64::try_from(id).unwrap_or(i64::MAX),
                i64::try_from(added).unwrap_or(i64::MAX)
            ],
        )
        .map_err(storage)?;
        bump(tx)?;
    }
    Ok(added)
}

/// The devices found linked to the user of a request that is not yet
/// complete.
pub(crate) fn anons(tx: &Transaction<'_>, id: u64) -> Result<Vec<String>, MetaError> {
    get(tx, id)?;
    let mut statement = tx
        .prepare("SELECT anon_id FROM erasure_anons WHERE request_id = ?1 ORDER BY anon_id")
        .map_err(storage)?;
    let rows = statement
        .query_map([i64::try_from(id).unwrap_or(i64::MAX)], |row| row.get(0))
        .map_err(storage)?;
    rows.map(|row| row.map_err(storage)).collect()
}

/// Moves a request forward: `requested` to `hidden` to `complete`, or to
/// `failed` from either. Completing clears the user id; the hash stays.
pub(crate) fn advance(
    tx: &Transaction<'_>,
    id: u64,
    to: ErasureState,
    detail: Option<&str>,
    at: i64,
) -> Result<ErasureRequest, MetaError> {
    let current = get(tx, id)?;
    let allowed = matches!(
        (current.state, to),
        (ErasureState::Requested, ErasureState::Hidden)
            | (ErasureState::Hidden, ErasureState::Complete)
            | (
                ErasureState::Requested | ErasureState::Hidden,
                ErasureState::Failed
            )
    );
    if !allowed {
        return Err(MetaError::Conflict(format!(
            "a request cannot go from {} to {}",
            current.state, to
        )));
    }
    let detail = detail.map(|text| text.chars().take(512).collect::<String>());
    let (hidden, completed) = match to {
        ErasureState::Hidden => (Some(at), None),
        ErasureState::Complete => (None, Some(at)),
        _ => (None, None),
    };
    tx.execute(
        "UPDATE erasure_requests SET state = ?2,
            hidden_at = COALESCE(?3, hidden_at),
            completed_at = COALESCE(?4, completed_at),
            detail = COALESCE(?5, detail),
            user_id = CASE WHEN ?2 = 'complete' THEN NULL ELSE user_id END,
            attempts = 0, retry_after = NULL,
            claimed_by = NULL, lease_until = NULL
         WHERE id = ?1",
        params![
            i64::try_from(id).unwrap_or(i64::MAX),
            to.as_str(),
            hidden,
            completed,
            detail
        ],
    )
    .map_err(storage)?;
    if to == ErasureState::Complete {
        tx.execute(
            "DELETE FROM erasure_anons WHERE request_id = ?1",
            [i64::try_from(id).unwrap_or(i64::MAX)],
        )
        .map_err(storage)?;
    }
    get(tx, id)
}

/// Puts a failed request back to `requested` so the job tries again.
pub(crate) fn retry(tx: &Transaction<'_>, id: u64) -> Result<ErasureRequest, MetaError> {
    let current = get(tx, id)?;
    if current.state != ErasureState::Failed {
        return Err(MetaError::Conflict(
            "only a failed request can be retried".to_owned(),
        ));
    }
    tx.execute(
        "UPDATE erasure_requests SET state = 'requested', detail = NULL,
            attempts = 0, retry_after = NULL,
            claimed_by = NULL, lease_until = NULL WHERE id = ?1",
        [i64::try_from(id).unwrap_or(i64::MAX)],
    )
    .map_err(storage)?;
    get(tx, id)
}

/// The tombstones made after position `after`, oldest first, at most `limit`.
pub(crate) fn tombstones_since(
    tx: &Transaction<'_>,
    key: Option<&HashKey>,
    after: u64,
    limit: usize,
) -> Result<TombstoneBatch, MetaError> {
    check_key_for_reading(tx, key)?;
    let mut statement = tx
        .prepare(
            "SELECT rowid, site_id, kind, hash FROM erasure_tombstones
             WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )
        .map_err(storage)?;
    let rows = statement
        .query_map(
            params![
                i64::try_from(after).unwrap_or(i64::MAX),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(storage)?;
    let mut batch = TombstoneBatch {
        last: after,
        entries: Vec::new(),
    };
    for row in rows {
        let (position, site, kind, hash) = row.map_err(storage)?;
        let kind = match kind.as_str() {
            "user" => IdKind::User,
            "anon" => IdKind::Anon,
            other => {
                return Err(MetaError::Corrupt(format!(
                    "unknown tombstone kind `{other}`"
                )));
            }
        };
        batch.last = u64::try_from(position).unwrap_or(batch.last);
        batch.entries.push((id_of(site)?, kind, hash));
    }
    Ok(batch)
}

/// Records trouble that retrying may fix: the request is let go, not tried
/// again for `retry_in_secs`, and after `max_attempts` such troubles in a row
/// it fails with the last message instead.
pub(crate) fn trouble(
    tx: &Transaction<'_>,
    id: u64,
    message: &str,
    retry_in_secs: i64,
    max_attempts: u32,
    at: i64,
) -> Result<ErasureRequest, MetaError> {
    let current = get(tx, id)?;
    if !matches!(
        current.state,
        ErasureState::Requested | ErasureState::Hidden
    ) {
        return Err(MetaError::Conflict(
            "only a request that is not finished can meet trouble".to_owned(),
        ));
    }
    let attempts = current.attempts.saturating_add(1);
    let message: String = message.chars().take(300).collect();
    let id_sql = i64::try_from(id).unwrap_or(i64::MAX);
    if attempts >= max_attempts.max(1) {
        tx.execute(
            "UPDATE erasure_requests SET state = 'failed', attempts = ?2, retry_after = NULL,
                detail = ?3, claimed_by = NULL, lease_until = NULL WHERE id = ?1",
            params![
                id_sql,
                attempts,
                format!("gave up after {attempts} attempts: {message}")
            ],
        )
        .map_err(storage)?;
    } else {
        tx.execute(
            "UPDATE erasure_requests SET attempts = ?2, retry_after = ?3, detail = ?4,
                claimed_by = NULL, lease_until = NULL WHERE id = ?1",
            params![
                id_sql,
                attempts,
                at.saturating_add(retry_in_secs.max(0)),
                format!("attempt {attempts} of {max_attempts} failed: {message}")
            ],
        )
        .map_err(storage)?;
    }
    get(tx, id)
}

pub(crate) fn summary(tx: &Transaction<'_>, at: i64) -> Result<ErasureSummary, MetaError> {
    let mut summary = ErasureSummary::default();
    let mut statement = tx
        .prepare("SELECT state, COUNT(*) FROM erasure_requests GROUP BY state")
        .map_err(storage)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(storage)?;
    for row in rows {
        let (state, count) = row.map_err(storage)?;
        let count = u64::try_from(count).unwrap_or(0);
        match ErasureState::parse(&state) {
            Some(ErasureState::Requested) => summary.requested = count,
            Some(ErasureState::Hidden) => summary.hidden = count,
            Some(ErasureState::Complete) => summary.complete = count,
            Some(ErasureState::Failed) => summary.failed = count,
            None => {
                return Err(MetaError::Corrupt(format!(
                    "unknown erasure state `{state}`"
                )));
            }
        }
    }
    summary.retrying = tx
        .query_row(
            "SELECT COUNT(*) FROM erasure_requests
             WHERE state IN ('requested', 'hidden') AND retry_after > ?1",
            [at],
            |row| row.get::<_, i64>(0),
        )
        .map_err(storage)
        .map(|n| u64::try_from(n).unwrap_or(0))?;
    summary.oldest_open_at = tx
        .query_row(
            "SELECT MIN(requested_at) FROM erasure_requests
             WHERE state IN ('requested', 'hidden')",
            [],
            |row| row.get(0),
        )
        .map_err(storage)?;
    Ok(summary)
}

/// Takes or renews the named lease for `holder`. It succeeds when nobody holds
/// it, when `holder` does, or when the holder's time ran out.
pub(crate) fn acquire_lease(
    tx: &Transaction<'_>,
    name: &str,
    holder: &str,
    lease_secs: i64,
    at: i64,
) -> Result<bool, MetaError> {
    let changed = tx
        .execute(
            "INSERT INTO leases (name, holder, until) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET holder = ?2, until = ?3
             WHERE leases.holder = ?2 OR leases.until <= ?4",
            params![name, holder, at.saturating_add(lease_secs.max(1)), at],
        )
        .map_err(storage)?;
    Ok(changed > 0)
}

/// Lets go of a lease `holder` holds, so another node can take it at once.
pub(crate) fn release_lease(
    tx: &Transaction<'_>,
    name: &str,
    holder: &str,
) -> Result<(), MetaError> {
    tx.execute(
        "DELETE FROM leases WHERE name = ?1 AND holder = ?2",
        params![name, holder],
    )
    .map_err(storage)?;
    Ok(())
}
