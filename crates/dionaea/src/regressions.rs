//! The regression job: once per interval it looks at the resolved issues and
//! reopens those whose error came back in a later release.
//!
//! A release is later than another when both labels are versions and the
//! first is the higher version (`1.10.0` after `1.9.0`, a hotfix `1.2.1`
//! shipped after `1.3.0` is still before it); when either label is not a
//! version, when it was first received decides. An error counts only when it
//! was received after the issue was resolved, and an error in the release
//! the issue was resolved in does not reopen it. One server does each run: a
//! run is claimed in the metadata store for the interval. Each run looks at
//! a bounded number of issues, the one checked longest ago first, so a large
//! backlog is worked through over several runs; the releases of one error
//! are read page by page, so no number of releases is left out. An issue is
//! looked at in depth only when the number of its errors changed since the
//! last check of the same decision; otherwise one count is all it costs.

use std::cmp::Ordering as Order;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use dionaea_meta::{IssueIndex, IssueRecord, MetaStore, compare_versions};
use dionaea_store::{Error, RELEASES_PER_PAGE, Sighting, Store};
use tracing::{info, warn};

const JOB: &str = "regressions";
/// How long after a failed run the next try is made.
const RETRY_AFTER_SECS: i64 = 120;

/// What the job needs from ClickHouse. [`Store`] is the implementation; tests
/// use their own.
pub(crate) trait ErrorLookup: Send + Sync {
    /// One page of the releases an error was received in since `since_ms`,
    /// in name order, after `after`.
    fn error_releases(
        &self,
        site_id: u32,
        fingerprint: &str,
        since_ms: i64,
        after: &str,
    ) -> Result<Vec<Sighting>, Error>;
    fn release_first_seen(&self, site_id: u32, release: &str) -> Result<Option<i64>, Error>;
    /// How many errors of the issue were received since `since_ms`.
    fn error_count(&self, site_id: u32, fingerprint: &str, since_ms: i64) -> Result<u64, Error>;
}

impl ErrorLookup for Store {
    fn error_releases(
        &self,
        site_id: u32,
        fingerprint: &str,
        since_ms: i64,
        after: &str,
    ) -> Result<Vec<Sighting>, Error> {
        Store::error_releases(self, site_id, fingerprint, since_ms, after)
    }

    fn error_count(&self, site_id: u32, fingerprint: &str, since_ms: i64) -> Result<u64, Error> {
        Store::error_count(self, site_id, fingerprint, since_ms)
    }

    fn release_first_seen(&self, site_id: u32, release: &str) -> Result<Option<i64>, Error> {
        Store::release_first_seen(self, site_id, release)
    }
}

/// What the job did, for the metrics.
#[derive(Debug, Default)]
pub(crate) struct RegressionStats {
    pub(crate) runs: AtomicU64,
    pub(crate) failures: AtomicU64,
    /// The last run failed.
    pub(crate) failing: AtomicBool,
    pub(crate) issues_checked: AtomicU64,
    pub(crate) issues_reopened: AtomicU64,
    /// When the last successful run finished, in seconds since the epoch (0
    /// when none has).
    pub(crate) last_success: AtomicI64,
}

pub(crate) struct Job {
    meta: Arc<dyn MetaStore>,
    issues: Arc<dyn IssueIndex>,
    lookup: Arc<dyn ErrorLookup>,
    stats: Arc<RegressionStats>,
}

/// Whether the label `candidate` is a later release than `resolved`, when
/// both labels are versions and so say their order; `None` when they do not.
fn later_by_label(candidate: &str, resolved: &str) -> Option<bool> {
    compare_versions(candidate, resolved).map(|order| order == Order::Greater)
}

/// Whether `candidate` was first received after `base_ms` (a release not
/// known to have been received is not later).
fn later_by_time(candidate: &Sighting, base_ms: i64) -> bool {
    candidate.first_ms.is_some_and(|first| first > base_ms)
}

/// Of two later releases, the one that came first (unknown times last, then
/// by name), so the report names the release the issue came back in.
fn earlier(a: Sighting, b: Sighting) -> Sighting {
    let key = |s: &Sighting| (s.first_ms.unwrap_or(i64::MAX), s.release.clone());
    if key(&b) < key(&a) { b } else { a }
}

impl Job {
    pub(crate) fn new(
        meta: Arc<dyn MetaStore>,
        issues: Arc<dyn IssueIndex>,
        lookup: Arc<dyn ErrorLookup>,
        stats: Arc<RegressionStats>,
    ) -> Self {
        Self {
            meta,
            issues,
            lookup,
            stats,
        }
    }

    /// Runs if it is due and no other server has taken the run. Returns
    /// whether this call ran it.
    pub(crate) fn run_if_due(&self, every: Duration, batch: usize, now_ms: u64) -> bool {
        let every_secs = i64::try_from(every.as_secs()).unwrap_or(i64::MAX);
        match self.meta.claim_run(JOB, every_secs) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                warn!(%error, "cannot tell whether the regression job is due");
                return false;
            }
        }
        self.stats.runs.fetch_add(1, Ordering::Relaxed);
        match self.run(batch) {
            Ok(()) => {
                self.stats.failing.store(false, Ordering::Relaxed);
                self.stats.last_success.store(
                    i64::try_from(now_ms / 1000).unwrap_or(i64::MAX),
                    Ordering::Relaxed,
                );
            }
            Err(message) => {
                self.stats.failures.fetch_add(1, Ordering::Relaxed);
                self.stats.failing.store(true, Ordering::Relaxed);
                warn!(error = %message, "the regression job failed; it will be tried again");
                if let Err(error) = self.meta.postpone_run(JOB, every_secs, RETRY_AFTER_SECS) {
                    warn!(%error, "cannot schedule the regression job to be tried again");
                }
            }
        }
        true
    }

    fn run(&self, batch: usize) -> Result<(), String> {
        let resolved = self
            .issues
            .resolved_to_check(batch)
            .map_err(|e| e.to_string())?;
        for issue in &resolved {
            self.check(issue)
                .map_err(|e| format!("site {} issue {}: {e}", issue.site_id, issue.fingerprint))?;
            self.stats.issues_checked.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn check(&self, issue: &IssueRecord) -> Result<(), String> {
        let (Some(resolved_release), Some(resolved_at)) =
            (issue.resolved_release.as_deref(), issue.resolved_at)
        else {
            return Ok(());
        };
        let site = issue.site_id;
        let since_ms = resolved_at.saturating_mul(1000);
        // Nothing to look at unless the errors of the issue changed since the
        // last check of this decision. The count is read first, so an error
        // that arrives while the releases are read is seen at the next check.
        let count = self
            .lookup
            .error_count(site, &issue.fingerprint, since_ms)
            .map_err(|e| e.to_string())?;
        if count == 0 || issue.errors_seen == Some(count) {
            return self.checked(issue, count);
        }
        // A release never received (the label was typed by a person) is
        // placed at the moment the issue was resolved; it is only needed for
        // releases whose labels do not say their order.
        let mut base_ms: Option<i64> = None;
        let mut found: Option<Sighting> = None;
        let mut after = String::new();
        loop {
            let page = self
                .lookup
                .error_releases(site, &issue.fingerprint, since_ms, &after)
                .map_err(|e| e.to_string())?;
            let full = page.len() >= RELEASES_PER_PAGE as usize;
            for sighting in page {
                // Names only move forward, so a lookup that ignored `after`
                // cannot keep the loop going.
                if sighting.release <= after && !after.is_empty() {
                    return Err("the releases did not come in name order".to_owned());
                }
                after.clone_from(&sighting.release);
                if sighting.release == resolved_release {
                    continue;
                }
                let later = match later_by_label(&sighting.release, resolved_release) {
                    Some(later) => later,
                    None => {
                        let base = match base_ms {
                            Some(base) => base,
                            None => {
                                let base = self
                                    .lookup
                                    .release_first_seen(site, resolved_release)
                                    .map_err(|e| e.to_string())?
                                    .unwrap_or(since_ms);
                                base_ms = Some(base);
                                base
                            }
                        };
                        later_by_time(&sighting, base)
                    }
                };
                if later {
                    found = Some(match found.take() {
                        Some(best) => earlier(best, sighting),
                        None => sighting,
                    });
                }
            }
            if !full {
                break;
            }
        }
        if let Some(sighting) = found {
            let reopened = self
                .issues
                .reopen_regressed(site, &issue.fingerprint, issue.revision, &sighting.release)
                .map_err(|e| e.to_string())?;
            if reopened {
                self.stats.issues_reopened.fetch_add(1, Ordering::Relaxed);
                info!(site, fingerprint = %issue.fingerprint, release = %sighting.release, "a resolved issue came back");
            }
            return Ok(());
        }
        self.checked(issue, count)
    }

    /// Records that the issue was looked at with this many errors.
    fn checked(&self, issue: &IssueRecord, count: u64) -> Result<(), String> {
        self.issues
            .mark_issue_checked(issue.site_id, &issue.fingerprint, issue.revision, count)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicI64;

    use dionaea_meta::{IssueState, SqliteMeta};

    use super::*;

    const EVERY: Duration = Duration::from_secs(60);
    const FP: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Default)]
    struct Fake {
        /// The releases the error was seen in, with when each was first
        /// received (`None`: unknown).
        seen: Mutex<BTreeMap<String, Option<i64>>>,
        /// When releases were first received, for the base lookup.
        firsts: Mutex<BTreeMap<String, i64>>,
        /// How many errors there are; the number of releases when not set.
        errors: Mutex<Option<u64>>,
        down: Mutex<bool>,
        since: Mutex<Vec<i64>>,
        pages: Mutex<u32>,
        base_lookups: Mutex<u32>,
    }

    impl ErrorLookup for Fake {
        fn error_releases(
            &self,
            _site_id: u32,
            _fingerprint: &str,
            since_ms: i64,
            after: &str,
        ) -> Result<Vec<Sighting>, Error> {
            if *self.down.lock().unwrap() {
                return Err(Error::Io("clickhouse is down".to_owned()));
            }
            self.since.lock().unwrap().push(since_ms);
            *self.pages.lock().unwrap() += 1;
            Ok(self
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(release, _)| release.as_str() > after)
                .take(RELEASES_PER_PAGE as usize)
                .map(|(release, first_ms)| Sighting {
                    release: release.clone(),
                    first_ms: *first_ms,
                })
                .collect())
        }

        fn release_first_seen(&self, _site_id: u32, release: &str) -> Result<Option<i64>, Error> {
            *self.base_lookups.lock().unwrap() += 1;
            Ok(self.firsts.lock().unwrap().get(release).copied())
        }

        fn error_count(&self, _: u32, _: &str, _: i64) -> Result<u64, Error> {
            if *self.down.lock().unwrap() {
                return Err(Error::Io("clickhouse is down".to_owned()));
            }
            Ok((*self.errors.lock().unwrap())
                .unwrap_or_else(|| self.seen.lock().unwrap().len() as u64))
        }
    }

    struct Rig {
        meta: Arc<SqliteMeta>,
        fake: Arc<Fake>,
        stats: Arc<RegressionStats>,
        job: Job,
        now: Arc<AtomicI64>,
        site: u32,
    }

    fn rig() -> Rig {
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = Arc::clone(&now);
        let meta = Arc::new(
            SqliteMeta::open_in_memory()
                .expect("open")
                .with_clock(move || clock.load(Ordering::Relaxed)),
        );
        let org = meta.create_organization("acme").expect("org");
        let site = meta.create_site(org.id, "app").expect("site").id;
        let fake = Arc::new(Fake::default());
        let stats = Arc::new(RegressionStats::default());
        let job = Job::new(
            Arc::clone(&meta) as Arc<dyn MetaStore>,
            Arc::clone(&meta) as Arc<dyn IssueIndex>,
            Arc::clone(&fake) as Arc<dyn ErrorLookup>,
            Arc::clone(&stats),
        );
        Rig {
            meta,
            fake,
            stats,
            job,
            now,
            site,
        }
    }

    impl Rig {
        fn resolve(&self, release: &str) {
            self.meta
                .set_issue_state(self.site, FP, IssueState::Resolved, Some(release))
                .expect("resolve");
        }

        /// The error was seen in this release, first received at `ms`.
        fn seen(&self, release: &str, ms: Option<i64>) {
            self.fake
                .seen
                .lock()
                .unwrap()
                .insert(release.to_owned(), ms);
        }

        fn first(&self, release: &str, ms: i64) {
            self.fake
                .firsts
                .lock()
                .unwrap()
                .insert(release.to_owned(), ms);
        }

        fn state(&self) -> IssueRecord {
            self.meta
                .issue(self.site, FP)
                .expect("read")
                .expect("a row")
        }

        fn pages(&self) -> u32 {
            *self.fake.pages.lock().unwrap()
        }

        /// Lets the interval pass and runs.
        fn run(&self) -> bool {
            self.now.fetch_add(100, Ordering::Relaxed);
            self.job.run_if_due(EVERY, 100, 5_000_000)
        }
    }

    fn sighting(release: &str, first_ms: Option<i64>) -> Sighting {
        Sighting {
            release: release.to_owned(),
            first_ms,
        }
    }

    #[test]
    fn versions_decide_when_both_labels_are_versions() {
        // A hotfix of an older line, shipped later: before 1.3.0.
        assert_eq!(later_by_label("1.2.1", "1.3.0"), Some(false));
        assert_eq!(later_by_label("1.10.0", "1.3.0"), Some(true), "10 > 3");
        assert_eq!(later_by_label("1.3.0+0", "1.3.0"), Some(false), "equal");
        assert_eq!(later_by_label("1.4.2+58", "1.4.2+57"), Some(true));
        // Not versions: the labels do not say.
        assert_eq!(later_by_label("def456", "abc123"), None);
        assert_eq!(later_by_label("nightly", "1.0.0"), None);
        assert_eq!(later_by_label("1.0.0", "2026-10-18"), None);
    }

    #[test]
    fn otherwise_the_release_first_received_later_is_later() {
        let base = 20_000;
        assert!(later_by_time(&sighting("def456", Some(30_000)), base));
        assert!(!later_by_time(&sighting("def456", Some(10_000)), base));
        assert!(!later_by_time(&sighting("def456", Some(20_000)), base));
        assert!(!later_by_time(&sighting("def456", None), base), "unknown");
    }

    #[test]
    fn of_two_later_releases_the_one_received_first_is_named() {
        let a = sighting("2.0.0", Some(40_000));
        let b = sighting("1.5.0", Some(30_000));
        assert_eq!(earlier(a.clone(), b.clone()), b);
        assert_eq!(earlier(b.clone(), a), b);
        let unknown = sighting("0.0.1", None);
        assert_eq!(earlier(unknown.clone(), b.clone()), b);
        let same_time = sighting("1.4.0", Some(30_000));
        assert_eq!(earlier(b.clone(), same_time.clone()), same_time, "by name");
    }

    #[test]
    fn an_error_in_a_later_release_reopens_the_issue() {
        let rig = rig();
        rig.resolve("1.2.0");
        rig.seen("1.3.0", Some(20_000));
        rig.seen("1.4.0", Some(30_000));
        assert!(rig.run());
        let issue = rig.state();
        assert_eq!(issue.state, IssueState::Unresolved);
        assert_eq!(issue.regressed_release.as_deref(), Some("1.3.0"));
        assert_eq!(issue.regressions, 1);
        assert_eq!(rig.stats.issues_reopened.load(Ordering::Relaxed), 1);
        // The look-up started at the moment of the resolution.
        assert_eq!(rig.fake.since.lock().unwrap().as_slice(), [1_000_000]);
        // Open issues are not looked at again.
        assert!(rig.run());
        assert_eq!(rig.stats.issues_checked.load(Ordering::Relaxed), 1);
        // Versioned labels never needed the time of the resolving release.
        assert_eq!(*rig.fake.base_lookups.lock().unwrap(), 0);
    }

    #[test]
    fn the_same_release_or_an_earlier_one_does_not_reopen_it() {
        let rig = rig();
        rig.resolve("1.2.0");
        // The resolving release itself and an older one that late events
        // still report.
        rig.seen("1.2.0", Some(10_000));
        rig.seen("1.1.0", Some(5_000));
        assert!(rig.run());
        assert_eq!(rig.state().state, IssueState::Resolved);
        assert_eq!(rig.stats.issues_reopened.load(Ordering::Relaxed), 0);
        assert_eq!(rig.stats.issues_checked.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_hotfix_shipped_after_a_newer_release_is_not_a_regression() {
        let rig = rig();
        rig.resolve("1.3.0");
        // 1.2.1 was received for the first time after 1.3.0.
        rig.seen("1.2.1", Some(50_000));
        assert!(rig.run());
        assert_eq!(rig.state().state, IssueState::Resolved);
    }

    #[test]
    fn labels_that_are_not_versions_are_ordered_by_first_sight() {
        let rig = rig();
        rig.resolve("nightly-x");
        rig.first("nightly-x", 900_000);
        // Placed where the resolving release was first received, or at the
        // resolution when it never was (the next test): before it, not later.
        rig.seen("nightly-a", Some(500_000));
        assert!(rig.run());
        assert_eq!(rig.state().state, IssueState::Resolved);
        // Received after it: later.
        rig.seen("nightly-z", Some(2_000_000));
        assert!(rig.run());
        assert_eq!(rig.state().regressed_release.as_deref(), Some("nightly-z"));
        assert!(*rig.fake.base_lookups.lock().unwrap() >= 1);
    }

    #[test]
    fn a_resolving_release_never_received_is_placed_at_the_resolution() {
        let rig = rig();
        rig.resolve("nightly-x");
        rig.seen("nightly-a", Some(500_000));
        assert!(rig.run());
        assert_eq!(rig.state().state, IssueState::Resolved);
        rig.seen("nightly-z", Some(1_000_001));
        assert!(rig.run());
        assert_eq!(rig.state().regressed_release.as_deref(), Some("nightly-z"));
    }

    #[test]
    fn a_release_with_no_known_time_and_no_version_is_ignored() {
        let rig = rig();
        rig.resolve("1.2.0");
        rig.seen("ghost", None);
        assert!(rig.run());
        assert_eq!(rig.state().state, IssueState::Resolved);
    }

    #[test]
    fn every_release_of_an_error_is_read_however_many_there_are() {
        let rig = rig();
        rig.resolve("0.0.1");
        // Far more releases than a page, all earlier than the resolving one
        // but the very last by name, which is the only later one.
        let many = RELEASES_PER_PAGE as usize * 2 + 7;
        for n in 0..many {
            rig.seen(&format!("0.0.0+{n:05}"), Some(1));
        }
        rig.seen("9.9.9", Some(2));
        assert!(rig.run());
        assert_eq!(rig.state().regressed_release.as_deref(), Some("9.9.9"));
        assert_eq!(rig.pages(), 3, "two full pages and a short one");
    }

    #[test]
    fn an_issue_whose_errors_did_not_change_is_not_looked_at_in_depth_again() {
        let rig = rig();
        rig.resolve("1.2.0");
        rig.seen("1.1.0", Some(5_000));
        assert!(rig.run());
        assert_eq!(rig.pages(), 1, "the first check reads the releases");
        assert_eq!(rig.state().errors_seen, Some(1));
        // Nothing new: only the count is asked.
        assert!(rig.run());
        assert!(rig.run());
        assert_eq!(rig.pages(), 1);
        assert_eq!(rig.stats.issues_checked.load(Ordering::Relaxed), 3);
        // A new error (the count changes): the releases are read again.
        *rig.fake.errors.lock().unwrap() = Some(2);
        rig.seen("1.3.0", Some(20_000));
        assert!(rig.run());
        assert_eq!(rig.pages(), 2);
        assert_eq!(rig.state().regressed_release.as_deref(), Some("1.3.0"));
    }

    #[test]
    fn an_issue_with_no_error_since_the_resolution_costs_one_count() {
        let rig = rig();
        rig.resolve("1.2.0");
        assert!(rig.run());
        assert_eq!(rig.pages(), 0);
        assert_eq!(rig.state().errors_seen, Some(0));
    }

    #[test]
    fn a_new_decision_by_a_person_makes_the_next_check_look_in_depth() {
        let rig = rig();
        rig.resolve("1.2.0");
        rig.seen("1.1.0", Some(5_000));
        assert!(rig.run());
        assert_eq!(rig.pages(), 1);
        rig.resolve("1.2.5");
        assert_eq!(rig.state().errors_seen, None);
        assert!(rig.run());
        assert_eq!(rig.pages(), 2, "same count, new decision");
    }

    #[test]
    fn a_lookup_that_ignores_the_page_marker_fails_the_run_instead_of_looping() {
        struct Stuck;
        impl ErrorLookup for Stuck {
            fn error_releases(
                &self,
                _: u32,
                _: &str,
                _: i64,
                _: &str,
            ) -> Result<Vec<Sighting>, Error> {
                Ok((0..RELEASES_PER_PAGE)
                    .map(|n| sighting(&format!("r{n:04}"), Some(1)))
                    .collect())
            }
            fn release_first_seen(&self, _: u32, _: &str) -> Result<Option<i64>, Error> {
                Ok(None)
            }
            fn error_count(&self, _: u32, _: &str, _: i64) -> Result<u64, Error> {
                Ok(1)
            }
        }
        let rig = rig();
        let job = Job::new(
            Arc::clone(&rig.meta) as Arc<dyn MetaStore>,
            Arc::clone(&rig.meta) as Arc<dyn IssueIndex>,
            Arc::new(Stuck),
            Arc::clone(&rig.stats),
        );
        rig.resolve("1.0.0");
        rig.now.fetch_add(100, Ordering::Relaxed);
        assert!(job.run_if_due(EVERY, 100, 5_000_000));
        assert_eq!(rig.stats.failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_person_who_resolves_again_while_a_run_is_checking_wins() {
        let rig = rig();
        rig.resolve("1.2.0");
        let stale = rig.state();
        // The same second: only the revision tells the decisions apart.
        rig.resolve("1.5.0");
        assert!(
            !rig.meta
                .reopen_regressed(rig.site, FP, stale.revision, "1.3.0")
                .expect("reopen")
        );
        assert_eq!(rig.state().resolved_release.as_deref(), Some("1.5.0"));
    }

    #[test]
    fn a_run_is_taken_once_per_interval_and_a_failure_is_retried_soon() {
        let rig = rig();
        rig.resolve("1.2.0");
        *rig.fake.down.lock().unwrap() = true;
        rig.seen("1.3.0", Some(20_000));
        assert!(rig.run());
        assert_eq!(rig.stats.failures.load(Ordering::Relaxed), 1);
        assert!(rig.stats.failing.load(Ordering::Relaxed));
        assert_eq!(
            rig.state().state,
            IssueState::Resolved,
            "nothing was changed"
        );
        // Not due again at once.
        assert!(!rig.job.run_if_due(EVERY, 100, 5_000_000));
        // Due after the short retry wait, not after the whole interval.
        rig.now.fetch_add(RETRY_AFTER_SECS + 1, Ordering::Relaxed);
        *rig.fake.down.lock().unwrap() = false;
        assert!(rig.job.run_if_due(EVERY, 100, 6_000_000));
        assert!(!rig.stats.failing.load(Ordering::Relaxed));
        assert_eq!(rig.stats.last_success.load(Ordering::Relaxed), 6_000);
        assert_eq!(rig.state().state, IssueState::Unresolved);
    }

    #[test]
    fn a_batch_is_bounded_and_the_rest_waits_for_the_next_run() {
        let rig = rig();
        for n in 0..5 {
            let fp = format!("{n:032x}");
            rig.meta
                .set_issue_state(rig.site, &fp, IssueState::Resolved, Some("1.0"))
                .expect("resolve");
            rig.now.fetch_add(1, Ordering::Relaxed);
        }
        rig.now.fetch_add(100, Ordering::Relaxed);
        assert!(rig.job.run_if_due(EVERY, 2, 5_000_000));
        assert_eq!(rig.stats.issues_checked.load(Ordering::Relaxed), 2);
        rig.now.fetch_add(100, Ordering::Relaxed);
        assert!(rig.job.run_if_due(EVERY, 2, 5_000_000));
        rig.now.fetch_add(100, Ordering::Relaxed);
        assert!(rig.job.run_if_due(EVERY, 2, 5_000_000));
        assert_eq!(
            rig.stats.issues_checked.load(Ordering::Relaxed),
            5 + 1,
            "the third run starts over with the one checked longest ago"
        );
    }
}
