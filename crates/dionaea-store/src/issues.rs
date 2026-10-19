//! Reads that tell whether a resolved issue came back: which releases an
//! error was seen in, and when each release was first received (from the
//! `releases` table, one row per release, so the cost follows the number of
//! releases and not the number of events).

use crate::report::{Builder, Limits, Statement};
use crate::{Error, Store};

/// Most releases one page of [`Store::error_releases`] returns.
pub const RELEASES_PER_PAGE: u32 = 500;

const LIMITS: Limits = Limits {
    max_execution_secs: 30,
    max_rows: RELEASES_PER_PAGE as u64,
};

/// A release an error was seen in and when that release was first received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub release: String,
    /// Milliseconds since the epoch; `None` when the release has no row in
    /// `releases` (it was dropped by its two-year rule).
    pub first_ms: Option<i64>,
}

/// One page of the releases an error was received in at or after `since_ms`,
/// in name order, starting after `after` (empty for the first page), without
/// the empty label. Columns: `release`, `first_ms` (null when unknown).
pub(crate) fn error_releases(
    site_id: u32,
    fingerprint: &str,
    since_ms: i64,
    after: &str,
) -> Statement {
    let mut b = Builder::default();
    let site = b.param("UInt32", site_id);
    let fingerprint = b.param("String", fingerprint);
    let since = b.time(since_ms);
    let after = b.param("String", after);
    let site_again = b.param("UInt32", site_id);
    let sql = format!(
        "WITH e AS (SELECT app_version AS release FROM errors \
               WHERE site_id = {site} AND fingerprint = {fingerprint} \
               AND received_at >= {since} AND app_version != '' AND app_version > {after} \
               GROUP BY app_version ORDER BY app_version LIMIT {RELEASES_PER_PAGE}) \
         SELECT e.release AS release, toUnixTimestamp64Milli(r.first) AS first_ms \
         FROM e LEFT JOIN (SELECT app_version AS release, min(first_seen) AS first FROM releases \
                    WHERE site_id = {site_again} AND app_version IN (SELECT release FROM e) \
                    GROUP BY app_version) AS r USING release \
         ORDER BY release SETTINGS join_use_nulls = 1"
    );
    b.finish(sql)
}

/// How many errors of the issue were received at or after `since_ms`. Column:
/// `n`. It reads the rows of one fingerprint only (the table is sorted by
/// site and fingerprint).
pub(crate) fn error_count(site_id: u32, fingerprint: &str, since_ms: i64) -> Statement {
    let mut b = Builder::default();
    let site = b.param("UInt32", site_id);
    let fingerprint = b.param("String", fingerprint);
    let since = b.time(since_ms);
    let sql = format!(
        "SELECT count() AS n FROM errors \
         WHERE site_id = {site} AND fingerprint = {fingerprint} AND received_at >= {since}"
    );
    b.finish(sql)
}

/// When the first event of a release was received, in milliseconds. Column:
/// `first_ms`; no row when the release is unknown.
pub(crate) fn release_first_seen(site_id: u32, release: &str) -> Statement {
    let mut b = Builder::default();
    let site = b.param("UInt32", site_id);
    let release = b.param("String", release);
    let sql = format!(
        "SELECT toUnixTimestamp64Milli(min(first_seen)) AS first_ms FROM releases \
         WHERE site_id = {site} AND app_version = {release} HAVING count() > 0"
    );
    b.finish(sql)
}

impl Store {
    /// One page (at most [`RELEASES_PER_PAGE`]) of the releases an error was
    /// received in since `since_ms`, after the release named `after`. A page
    /// shorter than the maximum is the last.
    pub fn error_releases(
        &self,
        site_id: u32,
        fingerprint: &str,
        since_ms: i64,
        after: &str,
    ) -> Result<Vec<Sighting>, Error> {
        let rows = self.select(
            &error_releases(site_id, fingerprint, since_ms, after),
            &LIMITS,
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(Sighting {
                    release: row.get("release")?.as_str()?.to_owned(),
                    first_ms: row.get("first_ms").and_then(serde_json::Value::as_i64),
                })
            })
            .collect())
    }

    /// How many errors of the issue were received at or after `since_ms`.
    pub fn error_count(
        &self,
        site_id: u32,
        fingerprint: &str,
        since_ms: i64,
    ) -> Result<u64, Error> {
        let rows = self.select(&error_count(site_id, fingerprint, since_ms), &LIMITS)?;
        rows.first()
            .and_then(|row| row.get("n"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| Error::InvalidRow("a count that is not a number".to_owned()))
    }

    /// When the first event of a release was received (milliseconds since
    /// the epoch), or `None` when the release is unknown.
    pub fn release_first_seen(&self, site_id: u32, release: &str) -> Result<Option<i64>, Error> {
        let rows = self.select(&release_first_seen(site_id, release), &LIMITS)?;
        Ok(rows
            .first()
            .and_then(|row| row.get("first_ms"))
            .and_then(serde_json::Value::as_i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_travel_as_parameters_never_in_the_text() {
        let nasty = "x' OR 1=1 --";
        for statement in [
            error_releases(7, nasty, 1_000, nasty),
            release_first_seen(7, nasty),
            error_count(7, nasty, 1_000),
        ] {
            assert!(!statement.sql.contains(nasty), "{}", statement.sql);
            assert!(statement.params.iter().any(|(_, value)| value == nasty));
        }
    }
}
