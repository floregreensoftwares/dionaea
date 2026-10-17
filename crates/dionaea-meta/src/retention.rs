//! How long each site keeps its data, and when scheduled jobs last ran.

use rusqlite::{OptionalExtension, Transaction, params};

use crate::sqlite::{id_of, storage};
use crate::{MetaError, Retention, RetentionFamily};

pub(crate) fn set(
    tx: &Transaction<'_>,
    site_id: u32,
    family: RetentionFamily,
    days: Option<u32>,
) -> Result<(), MetaError> {
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
    match days {
        Some(days) => {
            if !(crate::MIN_RETENTION_DAYS..=crate::MAX_RETENTION_DAYS).contains(&days) {
                return Err(MetaError::Invalid(format!(
                    "retention is {} to {} days",
                    crate::MIN_RETENTION_DAYS,
                    crate::MAX_RETENTION_DAYS
                )));
            }
            tx.execute(
                "INSERT INTO site_retention (site_id, family, days) VALUES (?1, ?2, ?3)
                 ON CONFLICT(site_id, family) DO UPDATE SET days = ?3",
                params![site_id, family.as_str(), days],
            )
            .map_err(storage)?;
        }
        None => {
            tx.execute(
                "DELETE FROM site_retention WHERE site_id = ?1 AND family = ?2",
                params![site_id, family.as_str()],
            )
            .map_err(storage)?;
        }
    }
    Ok(())
}

pub(crate) fn all(tx: &Transaction<'_>) -> Result<Vec<Retention>, MetaError> {
    let mut statement = tx
        .prepare("SELECT site_id, family, days FROM site_retention ORDER BY site_id, family")
        .map_err(storage)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(storage)?;
    rows.map(|row| {
        let (site, family, days) = row.map_err(storage)?;
        Ok(Retention {
            site_id: id_of(site)?,
            family: RetentionFamily::parse(&family).ok_or_else(|| {
                MetaError::Corrupt(format!("unknown retention family `{family}`"))
            })?,
            days: u32::try_from(days)
                .map_err(|_| MetaError::Corrupt("retention out of range".to_owned()))?,
        })
    })
    .collect()
}

/// Whether the job `name` is due: it has not run for `every_secs`. When it is,
/// the run is recorded as starting at `at`, so no other server takes it.
pub(crate) fn claim_run(
    tx: &Transaction<'_>,
    name: &str,
    every_secs: i64,
    at: i64,
) -> Result<bool, MetaError> {
    let last: Option<i64> = tx
        .query_row(
            "SELECT last_run FROM job_runs WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?;
    if last.is_some_and(|last| last.saturating_add(every_secs.max(1)) > at) {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO job_runs (name, last_run) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET last_run = ?2",
        params![name, at],
    )
    .map_err(storage)?;
    Ok(true)
}

/// A run that failed: it becomes due again after `retry_in_secs` instead of
/// after a whole interval.
pub(crate) fn postpone_run(
    tx: &Transaction<'_>,
    name: &str,
    every_secs: i64,
    retry_in_secs: i64,
    at: i64,
) -> Result<(), MetaError> {
    let last = at
        .saturating_sub(every_secs.max(1))
        .saturating_add(retry_in_secs.max(0));
    tx.execute(
        "INSERT INTO job_runs (name, last_run) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET last_run = ?2",
        params![name, last],
    )
    .map_err(storage)?;
    Ok(())
}

/// When the job `name` last started, if it ever did.
pub(crate) fn last_run(tx: &Transaction<'_>, name: &str) -> Result<Option<i64>, MetaError> {
    tx.query_row(
        "SELECT last_run FROM job_runs WHERE name = ?1",
        [name],
        |row| row.get(0),
    )
    .optional()
    .map_err(storage)
}
