//! The state of issues: which error groups a person resolved or ignored, and
//! which resolved ones came back. An issue nobody touched has no row and is
//! unresolved. The errors themselves are in ClickHouse; this index holds only
//! the decision and when it was made.

use rusqlite::{OptionalExtension, params};

use crate::MetaError;
use crate::sqlite::{SqliteMeta, id_of, storage};
use crate::symbols::is_release;

/// Length of a fingerprint, in lowercase hex characters.
pub const FINGERPRINT_CHARS: usize = 32;
/// Most issues one page of a listing returns.
pub const MAX_ISSUES_LISTED: usize = 1_000;
/// Most issues with a state one site keeps.
pub const MAX_ISSUES_PER_SITE: u64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueState {
    Unresolved,
    Resolved,
    Ignored,
}

impl IssueState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::Resolved => "resolved",
            Self::Ignored => "ignored",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "unresolved" => Some(Self::Unresolved),
            "resolved" => Some(Self::Resolved),
            "ignored" => Some(Self::Ignored),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRecord {
    pub site_id: u32,
    pub fingerprint: String,
    pub state: IssueState,
    /// The release the issue was resolved in; set while `state` is
    /// `Resolved` and kept after a regression.
    pub resolved_release: Option<String>,
    /// Seconds since the epoch.
    pub resolved_at: Option<i64>,
    /// The release in which a resolved issue came back, and when it was
    /// noticed; set by the last regression.
    pub regressed_release: Option<String>,
    pub regressed_at: Option<i64>,
    pub regressions: u32,
    /// Grows with every decision about the issue, so a finding made for an
    /// earlier decision can be told apart from the current one however close
    /// in time they are.
    pub revision: u64,
    /// How many errors of the issue the last check found (since the
    /// resolution); `None` before the first check of this decision.
    pub errors_seen: Option<u64>,
    pub checked_at: i64,
    pub updated_at: i64,
}

/// Whether `text` is a fingerprint as the health rules write it.
pub fn is_fingerprint(text: &str) -> bool {
    text.len() == FINGERPRINT_CHARS && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub trait IssueIndex: Send + Sync {
    /// Sets the state of an issue. `Resolved` needs the release it was fixed
    /// in; the other states take none. Resolving again replaces the release
    /// and clears the mark of an earlier regression. A site keeps at most
    /// [`MAX_ISSUES_PER_SITE`] issues with a state.
    fn set_issue_state(
        &self,
        site_id: u32,
        fingerprint: &str,
        state: IssueState,
        release: Option<&str>,
    ) -> Result<IssueRecord, MetaError>;

    fn issue(&self, site_id: u32, fingerprint: &str) -> Result<Option<IssueRecord>, MetaError>;

    /// A page of the issues of a site that have a row, of one state when
    /// given, by fingerprint, after the fingerprint `after` when given. At
    /// most `limit` (and [`MAX_ISSUES_LISTED`]); a page shorter than the
    /// limit is the last.
    fn issues(
        &self,
        site_id: u32,
        state: Option<IssueState>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IssueRecord>, MetaError>;

    /// Up to `limit` resolved issues of every site, the one checked longest
    /// ago first.
    fn resolved_to_check(&self, limit: usize) -> Result<Vec<IssueRecord>, MetaError>;

    /// Records that a resolved issue was looked at and how many errors it had,
    /// so the others are checked before it is again and it is looked at in
    /// depth only when the count changes. Does nothing when the issue was
    /// decided again (`revision`) since it was read.
    fn mark_issue_checked(
        &self,
        site_id: u32,
        fingerprint: &str,
        revision: u64,
        errors_seen: u64,
    ) -> Result<(), MetaError>;

    /// Reopens an issue that is still as it was when it was read
    /// (`revision`), because it appeared in `release`. Returns whether it
    /// did: a person who decided in between wins.
    fn reopen_regressed(
        &self,
        site_id: u32,
        fingerprint: &str,
        revision: u64,
        release: &str,
    ) -> Result<bool, MetaError>;
}

const COLUMNS: &str = "site_id, fingerprint, state, resolved_release, resolved_at, \
                       regressed_release, regressed_at, regressions, revision, errors_seen, \
                       checked_at, updated_at";

type Raw = (
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    i64,
    i64,
    i64,
    i64,
    i64,
);

fn raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn build(r: Raw) -> Result<IssueRecord, MetaError> {
    Ok(IssueRecord {
        site_id: id_of(r.0)?,
        fingerprint: r.1,
        state: IssueState::parse(&r.2)
            .ok_or_else(|| MetaError::Corrupt(format!("issue state `{}` is unknown", r.2)))?,
        resolved_release: r.3,
        resolved_at: r.4,
        regressed_release: r.5,
        regressed_at: r.6,
        regressions: u32::try_from(r.7).unwrap_or(u32::MAX),
        revision: u64::try_from(r.8).unwrap_or(0),
        errors_seen: u64::try_from(r.9).ok(),
        checked_at: r.10,
        updated_at: r.11,
    })
}

fn fetch(
    tx: &rusqlite::Transaction<'_>,
    site_id: u32,
    fingerprint: &str,
) -> Result<Option<IssueRecord>, MetaError> {
    tx.query_row(
        &format!("SELECT {COLUMNS} FROM issues WHERE site_id = ?1 AND fingerprint = ?2"),
        params![site_id, fingerprint],
        raw,
    )
    .optional()
    .map_err(storage)?
    .map(build)
    .transpose()
}

impl IssueIndex for SqliteMeta {
    fn set_issue_state(
        &self,
        site_id: u32,
        fingerprint: &str,
        state: IssueState,
        release: Option<&str>,
    ) -> Result<IssueRecord, MetaError> {
        if !is_fingerprint(fingerprint) {
            return Err(MetaError::Invalid(
                "a fingerprint is 32 lowercase hex characters".to_owned(),
            ));
        }
        match (state, release) {
            (IssueState::Resolved, Some(release)) if is_release(release) => {}
            (IssueState::Resolved, _) => {
                return Err(MetaError::Invalid(
                    "resolving an issue needs the release it was fixed in (1 to 64 characters of letters, digits and `_.+/-`)"
                        .to_owned(),
                ));
            }
            (_, None) => {}
            (_, Some(_)) => {
                return Err(MetaError::Invalid(
                    "only a resolved issue has a release".to_owned(),
                ));
            }
        }
        self.write(|tx, at| {
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
            if fetch(tx, site_id, fingerprint)?.is_none() {
                let held: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM issues WHERE site_id = ?1",
                        [site_id],
                        |row| row.get(0),
                    )
                    .map_err(storage)?;
                if u64::try_from(held).unwrap_or(u64::MAX) >= MAX_ISSUES_PER_SITE {
                    return Err(MetaError::Conflict(format!(
                        "a site keeps at most {MAX_ISSUES_PER_SITE} issues with a state"
                    )));
                }
            }
            let resolved_at = (state == IssueState::Resolved).then_some(at);
            tx.execute(
                "INSERT INTO issues (site_id, fingerprint, state, resolved_release, resolved_at,
                                     checked_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT (site_id, fingerprint) DO UPDATE SET
                     state = excluded.state,
                     resolved_release = excluded.resolved_release,
                     resolved_at = excluded.resolved_at,
                     regressed_release = NULL,
                     regressed_at = NULL,
                     revision = revision + 1,
                     errors_seen = -1,
                     checked_at = excluded.checked_at,
                     updated_at = excluded.updated_at",
                params![
                    site_id,
                    fingerprint,
                    state.as_str(),
                    release,
                    resolved_at,
                    at
                ],
            )
            .map_err(storage)?;
            fetch(tx, site_id, fingerprint)?.ok_or_else(|| {
                MetaError::Corrupt("an issue vanished inside its transaction".to_owned())
            })
        })
    }

    fn issue(&self, site_id: u32, fingerprint: &str) -> Result<Option<IssueRecord>, MetaError> {
        self.read(|tx| fetch(tx, site_id, fingerprint))
    }

    fn issues(
        &self,
        site_id: u32,
        state: Option<IssueState>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IssueRecord>, MetaError> {
        let limit = i64::try_from(limit.min(MAX_ISSUES_LISTED)).unwrap_or(i64::MAX);
        self.read(|tx| {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM issues
                     WHERE site_id = ?1 AND (?2 IS NULL OR state = ?2)
                       AND (?3 IS NULL OR fingerprint > ?3)
                     ORDER BY fingerprint LIMIT ?4"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map(
                    params![site_id, state.map(IssueState::as_str), after, limit],
                    raw,
                )
                .map_err(storage)?;
            rows.map(|row| build(row.map_err(storage)?)).collect()
        })
    }

    fn resolved_to_check(&self, limit: usize) -> Result<Vec<IssueRecord>, MetaError> {
        let limit = i64::try_from(limit.min(MAX_ISSUES_LISTED)).unwrap_or(i64::MAX);
        self.read(|tx| {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM issues WHERE state = 'resolved'
                     ORDER BY checked_at, site_id, fingerprint LIMIT ?1"
                ))
                .map_err(storage)?;
            let rows = statement.query_map([limit], raw).map_err(storage)?;
            rows.map(|row| build(row.map_err(storage)?)).collect()
        })
    }

    fn mark_issue_checked(
        &self,
        site_id: u32,
        fingerprint: &str,
        revision: u64,
        errors_seen: u64,
    ) -> Result<(), MetaError> {
        let revision = i64::try_from(revision).unwrap_or(i64::MAX);
        let errors_seen = i64::try_from(errors_seen).unwrap_or(i64::MAX);
        self.write(|tx, at| {
            tx.execute(
                "UPDATE issues SET checked_at = ?3, errors_seen = ?5
                 WHERE site_id = ?1 AND fingerprint = ?2 AND revision = ?4",
                params![site_id, fingerprint, at, revision, errors_seen],
            )
            .map_err(storage)?;
            Ok(())
        })
    }

    fn reopen_regressed(
        &self,
        site_id: u32,
        fingerprint: &str,
        revision: u64,
        release: &str,
    ) -> Result<bool, MetaError> {
        if !is_release(release) {
            return Err(MetaError::Invalid("not a release label".to_owned()));
        }
        let revision = i64::try_from(revision).unwrap_or(i64::MAX);
        self.write(|tx, at| {
            let changed = tx
                .execute(
                    "UPDATE issues SET state = 'unresolved', regressed_release = ?4,
                                       regressed_at = ?5, regressions = regressions + 1,
                                       revision = revision + 1, checked_at = ?5, updated_at = ?5
                     WHERE site_id = ?1 AND fingerprint = ?2
                       AND state = 'resolved' AND revision = ?3",
                    params![site_id, fingerprint, revision, release, at],
                )
                .map_err(storage)?;
            Ok(changed > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;
    use crate::MetaStore;

    const FP: &str = "0123456789abcdef0123456789abcdef";

    fn meta() -> (SqliteMeta, Arc<AtomicI64>, u32) {
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = Arc::clone(&now);
        let meta = SqliteMeta::open_in_memory()
            .expect("open")
            .with_clock(move || clock.load(Ordering::Relaxed));
        let org = meta.create_organization("acme").expect("org");
        let site = meta.create_site(org.id, "app").expect("site");
        (meta, now, site.id)
    }

    fn all(meta: &SqliteMeta, site: u32, state: Option<IssueState>) -> Vec<IssueRecord> {
        meta.issues(site, state, None, MAX_ISSUES_LISTED)
            .expect("list")
    }

    #[test]
    fn fingerprints_are_32_lowercase_hex() {
        assert!(is_fingerprint(FP));
        for bad in [
            "",
            "ABCDEF0123456789abcdef0123456789",
            &FP[..31],
            "g123456789abcdef0123456789abcdef",
        ] {
            assert!(!is_fingerprint(bad), "{bad}");
        }
    }

    #[test]
    fn an_untouched_issue_has_no_row() {
        let (meta, _, site) = meta();
        assert_eq!(meta.issue(site, FP).expect("read"), None);
        assert!(all(&meta, site, None).is_empty());
    }

    #[test]
    fn resolving_needs_a_release_and_the_others_refuse_one() {
        let (meta, _, site) = meta();
        for (state, release) in [
            (IssueState::Resolved, None),
            (IssueState::Resolved, Some("bad release")),
            (IssueState::Ignored, Some("1.0.0")),
            (IssueState::Unresolved, Some("1.0.0")),
        ] {
            assert!(matches!(
                meta.set_issue_state(site, FP, state, release),
                Err(MetaError::Invalid(_))
            ));
        }
        assert!(matches!(
            meta.set_issue_state(site, "nope", IssueState::Ignored, None),
            Err(MetaError::Invalid(_))
        ));
        assert!(matches!(
            meta.set_issue_state(site + 1, FP, IssueState::Ignored, None),
            Err(MetaError::NotFound("site"))
        ));
    }

    #[test]
    fn a_resolved_issue_reopens_once_and_a_new_resolution_clears_the_mark() {
        let (meta, now, site) = meta();
        let resolved = meta
            .set_issue_state(site, FP, IssueState::Resolved, Some("1.2.0"))
            .expect("resolve");
        assert_eq!(
            (resolved.resolved_release.as_deref(), resolved.resolved_at),
            (Some("1.2.0"), Some(1_000))
        );
        now.store(2_000, Ordering::Relaxed);
        assert!(
            meta.reopen_regressed(site, FP, resolved.revision, "1.3.0")
                .expect("reopen")
        );
        // Already open: the same finding does not count twice.
        assert!(
            !meta
                .reopen_regressed(site, FP, resolved.revision, "1.3.0")
                .expect("again")
        );
        let reopened = meta.issue(site, FP).expect("read").expect("row");
        assert_eq!(reopened.state, IssueState::Unresolved);
        assert_eq!(
            (
                reopened.regressed_release.as_deref(),
                reopened.regressed_at,
                reopened.regressions
            ),
            (Some("1.3.0"), Some(2_000), 1)
        );
        now.store(3_000, Ordering::Relaxed);
        let again = meta
            .set_issue_state(site, FP, IssueState::Resolved, Some("1.3.1"))
            .expect("resolve again");
        assert_eq!((again.regressed_at, again.regressions), (None, 1));
    }

    #[test]
    fn a_person_who_decided_meanwhile_wins_even_within_the_same_second() {
        let (meta, _, site) = meta();
        let first = meta
            .set_issue_state(site, FP, IssueState::Resolved, Some("1.2.0"))
            .expect("resolve");
        // The same second on the same clock: only the revision tells them apart.
        let second = meta
            .set_issue_state(site, FP, IssueState::Resolved, Some("1.5.0"))
            .expect("resolve again");
        assert_eq!(first.resolved_at, second.resolved_at);
        assert!(second.revision > first.revision);
        assert!(
            !meta
                .reopen_regressed(site, FP, first.revision, "1.3.0")
                .expect("stale")
        );
        meta.mark_issue_checked(site, FP, first.revision, 9)
            .expect("stale mark");
        let now = meta.issue(site, FP).expect("read").expect("row");
        assert_eq!(
            (now.state, now.resolved_release.as_deref(), now.errors_seen),
            (IssueState::Resolved, Some("1.5.0"), None),
            "neither the reopening nor the mark of the old decision landed"
        );
        // An ignore also ends the old finding.
        let ignored = meta
            .set_issue_state(site, FP, IssueState::Ignored, None)
            .expect("ignore");
        assert!(
            !meta
                .reopen_regressed(site, FP, second.revision, "1.3.0")
                .expect("reopen")
        );
        assert_eq!(ignored.state, IssueState::Ignored);
    }

    #[test]
    fn resolved_issues_are_checked_oldest_first_and_only_those() {
        let (meta, now, site) = meta();
        let other = "f".repeat(32);
        let third = "e".repeat(32);
        let a = meta
            .set_issue_state(site, FP, IssueState::Resolved, Some("1.0"))
            .expect("a");
        now.store(1_100, Ordering::Relaxed);
        meta.set_issue_state(site, &other, IssueState::Resolved, Some("1.0"))
            .expect("b");
        meta.set_issue_state(site, &third, IssueState::Ignored, None)
            .expect("c");
        let order = |limit| {
            meta.resolved_to_check(limit)
                .expect("list")
                .into_iter()
                .map(|i| i.fingerprint)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(10), [FP.to_owned(), other.clone()]);
        assert_eq!(order(1), [FP.to_owned()]);
        now.store(1_200, Ordering::Relaxed);
        meta.mark_issue_checked(site, FP, a.revision, 4)
            .expect("mark");
        assert_eq!(order(10), [other, FP.to_owned()]);
        let marked = meta.issue(site, FP).expect("read").expect("row");
        assert_eq!(marked.errors_seen, Some(4));
    }

    #[test]
    fn listing_filters_by_state_and_comes_in_pages() {
        let (meta, _, site) = meta();
        for n in 0..5u8 {
            let fp = format!("{n:032x}");
            meta.set_issue_state(site, &fp, IssueState::Ignored, None)
                .expect("ignore");
        }
        meta.set_issue_state(site, FP, IssueState::Resolved, Some("1.0"))
            .expect("resolve");
        assert_eq!(all(&meta, site, Some(IssueState::Ignored)).len(), 5);
        assert_eq!(all(&meta, site, Some(IssueState::Resolved)).len(), 1);
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = meta.issues(site, None, after.as_deref(), 2).expect("page");
            let last = page.len() < 2;
            if let Some(end) = page.last() {
                after = Some(end.fingerprint.clone());
            }
            seen.extend(page.into_iter().map(|i| i.fingerprint));
            if last {
                break;
            }
        }
        assert_eq!(seen.len(), 6, "every issue once");
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(sorted, seen, "by fingerprint");
    }

    #[test]
    fn a_site_keeps_a_bounded_number_of_issues_and_may_still_change_them() {
        let (meta, _, site) = meta();
        meta.write(|tx, _| {
            let mut insert = tx
                .prepare(
                    "INSERT INTO issues (site_id, fingerprint, state, updated_at)
                     VALUES (?1, ?2, 'ignored', 0)",
                )
                .map_err(storage)?;
            for n in 1..=MAX_ISSUES_PER_SITE {
                insert
                    .execute(params![site, format!("{n:032x}")])
                    .map_err(storage)?;
            }
            Ok(())
        })
        .expect("fill the site");
        assert!(matches!(
            meta.set_issue_state(site, FP, IssueState::Ignored, None),
            Err(MetaError::Conflict(_))
        ));
        // One that has a row can still change.
        let held = format!("{:032x}", 1);
        assert!(
            meta.set_issue_state(site, &held, IssueState::Resolved, Some("1.0"))
                .is_ok()
        );
    }
}
