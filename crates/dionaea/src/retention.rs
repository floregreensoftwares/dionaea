//! The retention job: once per interval it removes, for each site, the
//! analytics data older than the number of days the site keeps.
//!
//! One server does each run: a run is claimed in the metadata store for the
//! interval, so the others find it taken. A site deletes its own old rows; a
//! month that is older than every site's cutoff is dropped whole; and the
//! months that still hold deleted rows are rewritten so the data leaves the
//! disk. A run that fails is tried again after a short wait, not a whole
//! interval, and shows in the metrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use dionaea_meta::{MetaStore, RetentionFamily};
use dionaea_store::{Error, Expired, Rewrite, Store};
use tracing::{info, warn};

const JOB: &str = "retention";
/// How long after a failed run the next try is made.
const RETRY_AFTER_SECS: i64 = 900;
const DAY_MS: u64 = 86_400_000;

/// What the job needs from ClickHouse. [`Store`] is the implementation; tests
/// use their own.
pub(crate) trait Expirer: Send + Sync {
    fn expire(&self, site_id: u32, cutoff_ms: u64) -> Result<Vec<Expired>, Error>;
    fn drop_partitions(&self, cutoff_ms: u64) -> Result<Vec<(&'static str, u32)>, Error>;
    fn rewrite(&self, cutoff_ms: u64) -> Result<Vec<Rewrite>, Error>;
}

impl Expirer for Store {
    fn expire(&self, site_id: u32, cutoff_ms: u64) -> Result<Vec<Expired>, Error> {
        Store::expire_analytics(self, site_id, cutoff_ms)
    }

    fn drop_partitions(&self, cutoff_ms: u64) -> Result<Vec<(&'static str, u32)>, Error> {
        Store::drop_expired_partitions(self, cutoff_ms)
    }

    fn rewrite(&self, cutoff_ms: u64) -> Result<Vec<Rewrite>, Error> {
        Store::rewrite_expired(self, cutoff_ms)
    }
}

/// What the job did, for the metrics.
#[derive(Debug, Default)]
pub(crate) struct RetentionStats {
    pub(crate) runs: AtomicU64,
    pub(crate) failures: AtomicU64,
    /// The last run failed.
    pub(crate) failing: AtomicBool,
    pub(crate) rows_expired: AtomicU64,
    pub(crate) partitions_dropped: AtomicU64,
    pub(crate) partitions_rewritten: AtomicU64,
    /// When the last successful run finished, in seconds since the epoch (0
    /// when none has).
    pub(crate) last_success: AtomicI64,
}

pub(crate) struct Job {
    meta: Arc<dyn MetaStore>,
    expirer: Arc<dyn Expirer>,
    stats: Arc<RetentionStats>,
}

impl Job {
    pub(crate) fn new(
        meta: Arc<dyn MetaStore>,
        expirer: Arc<dyn Expirer>,
        stats: Arc<RetentionStats>,
    ) -> Self {
        Self {
            meta,
            expirer,
            stats,
        }
    }

    /// Runs if it is due and no other server has taken the run. Returns
    /// whether this call ran it.
    pub(crate) fn run_if_due(&self, every: Duration, default_days: u32, now_ms: u64) -> bool {
        let every_secs = i64::try_from(every.as_secs()).unwrap_or(i64::MAX);
        match self.meta.claim_run(JOB, every_secs) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                warn!(%error, "cannot tell whether the retention job is due");
                return false;
            }
        }
        self.stats.runs.fetch_add(1, Ordering::Relaxed);
        match self.run(default_days, now_ms) {
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
                warn!(error = %message, "the retention job failed; it will be tried again");
                if let Err(error) = self.meta.postpone_run(JOB, every_secs, RETRY_AFTER_SECS) {
                    warn!(%error, "cannot schedule the retention job to be tried again");
                }
            }
        }
        true
    }

    fn run(&self, default_days: u32, now_ms: u64) -> Result<(), String> {
        let sites = self.meta.sites().map_err(|e| e.to_string())?;
        let own = self.meta.retentions().map_err(|e| e.to_string())?;
        let cutoff = |site_id: u32| {
            let days = own
                .iter()
                .find(|r| r.site_id == site_id && r.family == RetentionFamily::Analytics)
                .map_or(default_days, |r| r.days);
            now_ms.saturating_sub(u64::from(days) * DAY_MS)
        };
        let mut oldest = u64::MAX;
        let mut newest = 0;
        for site in &sites {
            let at = cutoff(site.id);
            oldest = oldest.min(at);
            newest = newest.max(at);
            let expired = self
                .expirer
                .expire(site.id, at)
                .map_err(|e| format!("site {}: {e}", site.id))?;
            let rows: u64 = expired.iter().map(|e| e.rows).sum();
            self.stats.rows_expired.fetch_add(rows, Ordering::Relaxed);
            if rows > 0 {
                info!(site = site.id, rows, "expired data removed");
            }
        }
        if sites.is_empty() {
            return Ok(());
        }
        let dropped = self
            .expirer
            .drop_partitions(oldest)
            .map_err(|e| format!("dropping months: {e}"))?;
        self.stats
            .partitions_dropped
            .fetch_add(dropped.len() as u64, Ordering::Relaxed);
        let rewritten = self
            .expirer
            .rewrite(newest)
            .map_err(|e| format!("rewriting months: {e}"))?;
        self.stats
            .partitions_rewritten
            .fetch_add(rewritten.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dionaea_meta::SqliteMeta;

    use super::*;

    const EVERY: Duration = Duration::from_secs(1_000);
    const NOW: u64 = 1_760_000_000_000;

    #[derive(Default)]
    struct Fake {
        expired: Mutex<Vec<(u32, u64)>>,
        dropped: Mutex<Vec<u64>>,
        rewritten: Mutex<Vec<u64>>,
        fail_site: Mutex<Option<u32>>,
        fail_drop: Mutex<bool>,
    }

    impl Expirer for Fake {
        fn expire(&self, site_id: u32, cutoff_ms: u64) -> Result<Vec<Expired>, Error> {
            if *self.fail_site.lock().unwrap() == Some(site_id) {
                return Err(Error::Io("clickhouse is down".to_owned()));
            }
            self.expired.lock().unwrap().push((site_id, cutoff_ms));
            Ok(vec![Expired {
                table: "events",
                rows: 10,
            }])
        }

        fn drop_partitions(&self, cutoff_ms: u64) -> Result<Vec<(&'static str, u32)>, Error> {
            if *self.fail_drop.lock().unwrap() {
                return Err(Error::Io("timeout".to_owned()));
            }
            self.dropped.lock().unwrap().push(cutoff_ms);
            Ok(vec![("events", 202_508)])
        }

        fn rewrite(&self, cutoff_ms: u64) -> Result<Vec<Rewrite>, Error> {
            self.rewritten.lock().unwrap().push(cutoff_ms);
            Ok(vec![
                Rewrite {
                    table: "events",
                    partition: Some(202_509),
                },
                Rewrite {
                    table: "sessions",
                    partition: Some(202_509),
                },
            ])
        }
    }

    struct World {
        meta: Arc<SqliteMeta>,
        fake: Arc<Fake>,
        job: Job,
        stats: Arc<RetentionStats>,
        now: Arc<std::sync::atomic::AtomicI64>,
        sites: Vec<u32>,
    }

    fn world(sites: usize) -> World {
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = Arc::clone(&now);
        let meta = Arc::new(
            SqliteMeta::open_in_memory()
                .unwrap()
                .with_clock(move || clock.load(Ordering::SeqCst)),
        );
        let organization = meta.create_organization("o").unwrap();
        let ids: Vec<u32> = (0..sites)
            .map(|n| {
                meta.create_site(organization.id, &format!("s{n}"))
                    .unwrap()
                    .id
            })
            .collect();
        let fake = Arc::new(Fake::default());
        let stats = Arc::new(RetentionStats::default());
        let job = Job::new(
            Arc::clone(&meta) as Arc<dyn MetaStore>,
            Arc::clone(&fake) as Arc<dyn Expirer>,
            Arc::clone(&stats),
        );
        World {
            meta,
            fake,
            job,
            stats,
            now,
            sites: ids,
        }
    }

    fn days(n: u64) -> u64 {
        n * DAY_MS
    }

    #[test]
    fn each_site_is_cut_at_its_own_number_of_days_and_months_follow_the_extremes() {
        let w = world(3);
        w.meta
            .set_retention(w.sites[1], RetentionFamily::Analytics, Some(30))
            .unwrap();
        w.meta
            .set_retention(w.sites[2], RetentionFamily::Analytics, Some(3650))
            .unwrap();
        assert!(w.job.run_if_due(EVERY, 395, NOW));

        assert_eq!(
            *w.fake.expired.lock().unwrap(),
            vec![
                (w.sites[0], NOW - days(395)),
                (w.sites[1], NOW - days(30)),
                (w.sites[2], NOW - days(3650)),
            ]
        );
        // A month is dropped only if the site that keeps the longest is done
        // with it, and rewritten if any site's rows in it were deleted.
        assert_eq!(*w.fake.dropped.lock().unwrap(), vec![NOW - days(3650)]);
        assert_eq!(*w.fake.rewritten.lock().unwrap(), vec![NOW - days(30)]);
        assert_eq!(w.stats.rows_expired.load(Ordering::Relaxed), 30);
        assert_eq!(w.stats.partitions_dropped.load(Ordering::Relaxed), 1);
        assert_eq!(w.stats.partitions_rewritten.load(Ordering::Relaxed), 2);
        assert!(!w.stats.failing.load(Ordering::Relaxed));
        assert_eq!(w.stats.last_success.load(Ordering::Relaxed), 1_760_000_000);
    }

    #[test]
    fn a_site_that_goes_back_to_the_default_follows_it() {
        let w = world(1);
        w.meta
            .set_retention(w.sites[0], RetentionFamily::Analytics, Some(30))
            .unwrap();
        w.meta
            .set_retention(w.sites[0], RetentionFamily::Analytics, None)
            .unwrap();
        w.job.run_if_due(EVERY, 100, NOW);
        assert_eq!(
            *w.fake.expired.lock().unwrap(),
            vec![(w.sites[0], NOW - days(100))]
        );
    }

    #[test]
    fn it_runs_once_per_interval_and_only_on_one_server() {
        let w = world(1);
        let other = Job::new(
            Arc::clone(&w.meta) as Arc<dyn MetaStore>,
            Arc::clone(&w.fake) as Arc<dyn Expirer>,
            Arc::new(RetentionStats::default()),
        );
        assert!(w.job.run_if_due(EVERY, 395, NOW));
        assert!(!other.run_if_due(EVERY, 395, NOW), "taken by the first");
        assert!(!w.job.run_if_due(EVERY, 395, NOW), "not due again yet");
        assert_eq!(w.fake.expired.lock().unwrap().len(), 1);

        w.now.store(1_000 + 1_000, Ordering::SeqCst);
        assert!(other.run_if_due(EVERY, 395, NOW), "the next interval");
        assert_eq!(w.fake.expired.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_failed_run_is_counted_and_tried_again_soon_not_after_a_whole_interval() {
        let w = world(2);
        *w.fake.fail_site.lock().unwrap() = Some(w.sites[1]);
        assert!(w.job.run_if_due(EVERY, 395, NOW));
        assert!(w.stats.failing.load(Ordering::Relaxed));
        assert_eq!(w.stats.failures.load(Ordering::Relaxed), 1);
        assert_eq!(w.stats.last_success.load(Ordering::Relaxed), 0);
        assert!(
            w.fake.dropped.lock().unwrap().is_empty(),
            "nothing after a failure"
        );

        w.now.store(1_000 + RETRY_AFTER_SECS - 1, Ordering::SeqCst);
        assert!(!w.job.run_if_due(EVERY, 395, NOW));
        w.now.store(1_000 + RETRY_AFTER_SECS, Ordering::SeqCst);
        *w.fake.fail_site.lock().unwrap() = None;
        assert!(w.job.run_if_due(EVERY, 395, NOW));
        assert!(!w.stats.failing.load(Ordering::Relaxed), "recovered");
        assert_eq!(w.stats.runs.load(Ordering::Relaxed), 2);
        assert_eq!(w.stats.failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_failure_while_dropping_months_also_fails_the_run() {
        let w = world(1);
        *w.fake.fail_drop.lock().unwrap() = true;
        w.job.run_if_due(EVERY, 395, NOW);
        assert!(w.stats.failing.load(Ordering::Relaxed));
        assert!(w.fake.rewritten.lock().unwrap().is_empty());
    }

    #[test]
    fn with_no_site_nothing_is_dropped_because_nobody_keeps_anything_to_compare() {
        let w = world(0);
        assert!(w.job.run_if_due(EVERY, 395, NOW));
        assert!(w.fake.dropped.lock().unwrap().is_empty());
        assert!(w.fake.expired.lock().unwrap().is_empty());
        assert!(!w.stats.failing.load(Ordering::Relaxed));
    }

    #[test]
    fn a_young_installation_does_not_underflow_the_cutoff() {
        let w = world(1);
        w.job.run_if_due(EVERY, 3650, 5 * DAY_MS);
        assert_eq!(w.fake.expired.lock().unwrap()[0].1, 0);
    }
}
