//! The erasure job: takes requests recorded in the metadata store and removes
//! the user from ClickHouse in two steps.
//!
//! 1. **Hide.** For a `requested` request the job finds every device linked
//!    to the user, tombstones them (so no node accepts or inserts anything of
//!    theirs), waits until every node has had time to load the tombstones,
//!    then deletes the user's rows with a lightweight delete. Queries stop
//!    returning them at once. The request becomes `hidden`.
//! 2. **Finish.** Requests that have been hidden long enough are taken
//!    together: their rows are deleted once more (a node that was slow to
//!    load the tombstones may have inserted some), the deleted rows are
//!    removed from disk in one rewrite for all of them, and a direct count
//!    that includes marked rows must find none. Then the request is
//!    `complete`; if rows remain it is `failed` with the counts.
//!
//! A failure that retrying could fix (ClickHouse unreachable) delays the
//! request with a growing wait, and after `erase_max_attempts` of them in a
//! row fails it with the last message. One that cannot be fixed by retrying
//! fails the request at once with the reason.
//!
//! Only one server at a time does this work: they compete for a lease in the
//! metadata store, renewed while the work runs, and the others stand by until
//! the holder stops or dies.
//!
//! Nothing here logs an identifier: requests are named by their number.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use dionaea_meta::{ErasureRequest, ErasureState, ErasureSummary, MetaError, MetaStore};
use dionaea_store::{Error, Store, TableCount};
use tracing::{info, warn};

/// How long a worker holds a request while it hides it.
const HIDE_LEASE_SECS: i64 = 600;
/// How long a worker holds the requests it is finishing.
const FINISH_LEASE_SECS: i64 = 3600;
/// The lease that makes one server at a time do the work, how long it lasts
/// without being renewed, and how often it is renewed while work runs.
const LEADER_LEASE: &str = "erasure";
const LEADER_LEASE_SECS: i64 = 30;
const HEARTBEAT: Duration = Duration::from_secs(10);
/// The wait after the first trouble with a request; it doubles each time up to
/// the cap.
const RETRY_BASE_SECS: i64 = 30;
const RETRY_CAP_SECS: i64 = 3600;

/// How long to leave a request alone after its `attempts`-th trouble.
fn backoff(attempts: u32) -> i64 {
    RETRY_BASE_SECS
        .saturating_mul(1_i64 << attempts.saturating_sub(1).min(20))
        .min(RETRY_CAP_SECS)
}

/// What the job did, for the metrics.
#[derive(Debug, Default)]
pub(crate) struct JobStats {
    /// This server holds the lease and does the work.
    pub(crate) leader: AtomicBool,
    pub(crate) hidden: AtomicU64,
    pub(crate) completed: AtomicU64,
    /// Requests the job failed: rejected by ClickHouse, or rows that remained.
    pub(crate) failed: AtomicU64,
    /// Troubles that retrying may fix, and the requests given up on after too
    /// many of them.
    pub(crate) trouble: AtomicU64,
    pub(crate) gave_up: AtomicU64,
    /// The requests by state, as of the last round.
    pub(crate) summary: Mutex<ErasureSummary>,
}

impl JobStats {
    pub(crate) fn summary(&self) -> ErasureSummary {
        *self.summary.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the job needs from ClickHouse. [`Store`] is the implementation; tests
/// use their own.
pub(crate) trait Eraser: Send + Sync {
    fn linked_anon_ids(&self, site_id: u32, user_id: &str) -> Result<Vec<String>, Error>;
    fn count(
        &self,
        site_id: u32,
        user_id: &str,
        anon_ids: &[String],
        marked: bool,
    ) -> Result<Vec<TableCount>, Error>;
    fn delete(&self, site_id: u32, user_id: &str, anon_ids: &[String]) -> Result<(), Error>;
    fn remove_deleted(&self) -> Result<(), Error>;
}

impl Eraser for Store {
    fn linked_anon_ids(&self, site_id: u32, user_id: &str) -> Result<Vec<String>, Error> {
        Store::linked_anon_ids(self, site_id, user_id)
    }

    fn count(
        &self,
        site_id: u32,
        user_id: &str,
        anon_ids: &[String],
        marked: bool,
    ) -> Result<Vec<TableCount>, Error> {
        Store::count_user_rows(self, site_id, user_id, anon_ids, marked)
    }

    fn delete(&self, site_id: u32, user_id: &str, anon_ids: &[String]) -> Result<(), Error> {
        Store::delete_user_rows(self, site_id, user_id, anon_ids)
    }

    fn remove_deleted(&self) -> Result<(), Error> {
        Store::remove_deleted_rows(self)
    }
}

/// When the job acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timing {
    /// The wait between tombstoning and deleting.
    pub(crate) grace: Duration,
    /// How long a request stays `hidden` before the physical removal.
    pub(crate) finish_after: Duration,
    /// Troubles in a row after which a request fails.
    pub(crate) max_attempts: u32,
}

pub(crate) struct Job {
    meta: Arc<dyn MetaStore>,
    eraser: Arc<dyn Eraser>,
    worker: String,
    stats: Arc<JobStats>,
    heartbeat: Duration,
}

/// The rows per table, as text for a request's report.
fn describe(counts: &[TableCount]) -> String {
    counts
        .iter()
        .map(|c| format!("{} {}", c.table, c.rows))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Job {
    pub(crate) fn new(
        meta: Arc<dyn MetaStore>,
        eraser: Arc<dyn Eraser>,
        worker: impl Into<String>,
        stats: Arc<JobStats>,
    ) -> Self {
        Self {
            meta,
            eraser,
            worker: worker.into(),
            stats,
            heartbeat: HEARTBEAT,
        }
    }

    /// How often the lease is renewed while work runs; for tests.
    #[cfg(test)]
    fn with_heartbeat(mut self, every: Duration) -> Self {
        self.heartbeat = every;
        self
    }

    /// What to do with a step that failed: delay the request when the cause
    /// is transient (failing it after too many in a row), fail it when
    /// retrying cannot help.
    fn trouble(&self, request: &ErasureRequest, error: &Error, timing: Timing) {
        if error.is_permanent() {
            warn!(request = request.id, %error, "an erasure request cannot be carried out");
            self.fail(request.id, &format!("rejected by ClickHouse: {error}"));
            return;
        }
        self.stats.trouble.fetch_add(1, Ordering::Relaxed);
        let wait = backoff(request.attempts.saturating_add(1));
        match self.meta.record_erasure_trouble(
            request.id,
            &error.to_string(),
            wait,
            timing.max_attempts,
        ) {
            Ok(after) if after.state == ErasureState::Failed => {
                self.stats.gave_up.fetch_add(1, Ordering::Relaxed);
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                warn!(
                    request = request.id,
                    attempts = after.attempts,
                    "an erasure request failed too many times; it is given up"
                );
            }
            Ok(after) => warn!(
                request = request.id,
                attempts = after.attempts,
                wait_secs = wait,
                %error,
                "an erasure step failed; it will be tried again"
            ),
            Err(meta_error) => Self::meta_trouble(request.id, &meta_error),
        }
    }

    fn fail(&self, id: u64, why: &str) {
        match self
            .meta
            .advance_erasure(id, ErasureState::Failed, Some(why))
        {
            Ok(_) => {
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => warn!(request = id, %error, "cannot mark an erasure request as failed"),
        }
    }

    /// One round of work, if this server may do it: it must hold the lease
    /// that lets one server at a time work, which a heartbeat renews while the
    /// work runs. Requests that were just made are hidden, then the ones that
    /// are due are finished. `wait` sleeps for the grace period and returns
    /// `false` when the server is stopping.
    pub(crate) fn run_round(&self, timing: Timing, now_secs: i64, wait: &dyn Fn(Duration) -> bool) {
        match self.meta.erasure_summary() {
            Ok(summary) => {
                *self
                    .stats
                    .summary
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = summary;
            }
            Err(error) => warn!(%error, "cannot count the erasure requests"),
        }
        let lead = self
            .meta
            .try_lease(LEADER_LEASE, &self.worker, LEADER_LEASE_SECS);
        let leading = lead.as_ref().is_ok_and(|held| *held);
        self.stats.leader.store(leading, Ordering::Relaxed);
        if let Err(error) = &lead {
            warn!(%error, "cannot take part in choosing which server does the erasures");
        }
        if !leading {
            return;
        }
        let done = AtomicBool::new(false);
        let lost = AtomicBool::new(false);
        thread::scope(|scope| {
            let beat = scope.spawn(|| self.heartbeat(&done, &lost));
            // A server that lost the lease finishes the statement it is in and
            // starts nothing more.
            loop {
                if lost.load(Ordering::Acquire) || !self.hide_next(timing, now_secs, wait) {
                    break;
                }
            }
            if !lost.load(Ordering::Acquire) {
                self.finish_due(timing, now_secs);
            }
            done.store(true, Ordering::Release);
            beat.thread().unpark();
        });
        if lost.load(Ordering::Acquire) {
            self.stats.leader.store(false, Ordering::Relaxed);
        }
    }

    /// Renews the lease while the work runs, so a long statement does not let
    /// another server in. Sets `lost` if another server holds it by now.
    fn heartbeat(&self, done: &AtomicBool, lost: &AtomicBool) {
        loop {
            thread::park_timeout(self.heartbeat);
            if done.load(Ordering::Acquire) {
                return;
            }
            match self
                .meta
                .try_lease(LEADER_LEASE, &self.worker, LEADER_LEASE_SECS)
            {
                Ok(true) => {}
                Ok(false) => {
                    warn!("the erasure lease was taken over while work was running");
                    lost.store(true, Ordering::Release);
                }
                Err(error) => warn!(%error, "cannot renew the erasure lease"),
            }
        }
    }

    /// Lets go of the lease, so another server takes over at once. For a
    /// server that is stopping.
    pub(crate) fn step_down(&self) {
        self.stats.leader.store(false, Ordering::Relaxed);
        if let Err(error) = self.meta.release_lease(LEADER_LEASE, &self.worker) {
            warn!(%error, "cannot let go of the erasure lease");
        }
    }

    fn meta_trouble(id: u64, error: &MetaError) {
        warn!(request = id, %error, "cannot update an erasure request");
    }

    /// Hides the oldest request that was just made, if there is one. Returns
    /// whether there was one. `wait` sleeps for the grace period and returns
    /// `false` when the server is stopping, in which case the request is left
    /// for the next start.
    pub(crate) fn hide_next(
        &self,
        timing: Timing,
        now_secs: i64,
        wait: &dyn Fn(Duration) -> bool,
    ) -> bool {
        let request = match self.meta.claim_erasure(
            ErasureState::Requested,
            &self.worker,
            HIDE_LEASE_SECS,
            now_secs,
        ) {
            Ok(Some(request)) => request,
            Ok(None) => return false,
            Err(error) => {
                warn!(%error, "cannot look for erasure requests");
                return false;
            }
        };
        let Some(user) = request.user_id.clone() else {
            self.fail(request.id, "the request has no user id");
            return true;
        };

        // The devices of an earlier attempt that tombstoned them and deleted
        // the links before it stopped are still known to the metadata store.
        let mut anons = match self.meta.erasure_anons(request.id) {
            Ok(known) => known,
            Err(error) => {
                Self::meta_trouble(request.id, &error);
                return true;
            }
        };
        match self.eraser.linked_anon_ids(request.site_id, &user) {
            Ok(found) => anons.extend(found),
            Err(error) => {
                self.trouble(&request, &error, timing);
                return true;
            }
        }
        anons.sort();
        anons.dedup();
        if let Err(error) = self.meta.add_erasure_anons(request.id, &anons) {
            Self::meta_trouble(request.id, &error);
            return true;
        }

        if !wait(timing.grace) {
            return true;
        }

        let before = match self
            .eraser
            .count(request.site_id, &user, &anons, false)
            .and_then(|before| {
                self.eraser.delete(request.site_id, &user, &anons)?;
                Ok(before)
            }) {
            Ok(before) => before,
            Err(error) => {
                self.trouble(&request, &error, timing);
                return true;
            }
        };
        let report = format!("hidden: {}; devices {}", describe(&before), anons.len());
        match self
            .meta
            .advance_erasure(request.id, ErasureState::Hidden, Some(&report))
        {
            Ok(_) => {
                self.stats.hidden.fetch_add(1, Ordering::Relaxed);
                info!(request = request.id, "an erasure request is hidden");
            }
            Err(error) => Self::meta_trouble(request.id, &error),
        }
        true
    }

    /// Finishes every request that has been hidden for long enough, with one
    /// rewrite of the tables for all of them. Returns how many it completed.
    pub(crate) fn finish_due(&self, timing: Timing, now_secs: i64) -> usize {
        let due_before = now_secs
            .saturating_sub(i64::try_from(timing.finish_after.as_secs()).unwrap_or(i64::MAX));
        let mut taken: Vec<(ErasureRequest, Vec<String>)> = Vec::new();
        loop {
            let request = match self.meta.claim_erasure(
                ErasureState::Hidden,
                &self.worker,
                FINISH_LEASE_SECS,
                due_before,
            ) {
                Ok(Some(request)) => request,
                Ok(None) => break,
                Err(error) => {
                    warn!(%error, "cannot look for hidden erasure requests");
                    break;
                }
            };
            let anons = match self.meta.erasure_anons(request.id) {
                Ok(anons) => anons,
                Err(error) => {
                    Self::meta_trouble(request.id, &error);
                    continue;
                }
            };
            taken.push((request, anons));
        }
        if taken.is_empty() {
            return 0;
        }

        // Anything a slow node inserted after the first delete goes now.
        let mut ready = Vec::with_capacity(taken.len());
        for (request, anons) in taken {
            let Some(user) = request.user_id.clone() else {
                self.fail(request.id, "the request has no user id");
                continue;
            };
            match self.eraser.delete(request.site_id, &user, &anons) {
                Ok(()) => ready.push((request, user, anons)),
                Err(error) => self.trouble(&request, &error, timing),
            }
        }
        if ready.is_empty() {
            return 0;
        }
        if let Err(error) = self.eraser.remove_deleted() {
            for (request, ..) in &ready {
                self.trouble(request, &error, timing);
            }
            return 0;
        }

        let mut completed = 0;
        for (request, user, anons) in ready {
            let counts = match self.eraser.count(request.site_id, &user, &anons, true) {
                Ok(counts) => counts,
                Err(error) => {
                    self.trouble(&request, &error, timing);
                    continue;
                }
            };
            if counts.iter().all(|c| c.rows == 0) {
                let report = format!("verified: no row left in {}", table_names(&counts));
                match self
                    .meta
                    .advance_erasure(request.id, ErasureState::Complete, Some(&report))
                {
                    Ok(_) => {
                        completed += 1;
                        self.stats.completed.fetch_add(1, Ordering::Relaxed);
                        info!(request = request.id, "an erasure request is complete");
                    }
                    Err(error) => Self::meta_trouble(request.id, &error),
                }
            } else {
                self.fail(
                    request.id,
                    &format!("rows remain after the rewrite: {}", describe(&counts)),
                );
            }
        }
        completed
    }
}

fn table_names(counts: &[TableCount]) -> String {
    counts
        .iter()
        .map(|c| c.table)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use dionaea_meta::SqliteMeta;

    use super::*;

    fn tombstones_of(meta: &SqliteMeta) -> dionaea_meta::Tombstones {
        let all = dionaea_meta::Tombstones::default();
        all.load_from(meta).unwrap();
        all
    }

    const TABLES: [&str; 3] = ["events", "identity_alias", "user_traits"];

    #[derive(Clone)]
    struct Row {
        table: &'static str,
        site: u32,
        user: Option<&'static str>,
        anon: Option<&'static str>,
        marked: bool,
    }

    /// ClickHouse as the job sees it: rows that can be marked deleted, then
    /// removed by a rewrite, and failures that can be switched on.
    #[derive(Default)]
    struct Fake {
        rows: Mutex<Vec<Row>>,
        transient: Mutex<Option<Error>>,
        rewrites: AtomicUsize,
        deletes: AtomicUsize,
        /// Rows survive the rewrite, as if it did not work.
        broken_rewrite: Mutex<bool>,
        /// The next rewrite, and only it, fails.
        fail_rewrite: Mutex<Option<Error>>,
    }

    impl Fake {
        fn with(rows: &[Row]) -> Arc<Self> {
            let fake = Self::default();
            *fake.rows.lock().unwrap() = rows.to_vec();
            Arc::new(fake)
        }

        fn fail_next(&self, error: Error) {
            *self.transient.lock().unwrap() = Some(error);
        }

        fn failure(&self) -> Result<(), Error> {
            self.transient.lock().unwrap().take().map_or(Ok(()), Err)
        }

        fn matches(row: &Row, site: u32, user: &str, anons: &[String]) -> bool {
            row.site == site
                && (row.user == Some(user)
                    || row.anon.is_some_and(|anon| anons.iter().any(|a| a == anon)))
        }

        fn insert(&self, row: Row) {
            self.rows.lock().unwrap().push(row);
        }

        fn total(&self) -> usize {
            self.rows.lock().unwrap().len()
        }
    }

    impl Eraser for Fake {
        fn linked_anon_ids(&self, site: u32, user: &str) -> Result<Vec<String>, Error> {
            self.failure()?;
            let mut found: Vec<String> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.table == "identity_alias" && r.site == site && r.user == Some(user))
                .filter_map(|r| r.anon.map(str::to_owned))
                .collect();
            found.sort();
            found.dedup();
            Ok(found)
        }

        fn count(
            &self,
            site: u32,
            user: &str,
            anons: &[String],
            marked: bool,
        ) -> Result<Vec<TableCount>, Error> {
            self.failure()?;
            let rows = self.rows.lock().unwrap();
            Ok(TABLES
                .iter()
                .map(|table| TableCount {
                    table,
                    rows: rows
                        .iter()
                        .filter(|r| r.table == *table && Self::matches(r, site, user, anons))
                        .filter(|r| marked || !r.marked)
                        .count() as u64,
                })
                .collect())
        }

        fn delete(&self, site: u32, user: &str, anons: &[String]) -> Result<(), Error> {
            self.failure()?;
            self.deletes.fetch_add(1, Ordering::SeqCst);
            for row in self.rows.lock().unwrap().iter_mut() {
                if Self::matches(row, site, user, anons) {
                    row.marked = true;
                }
            }
            Ok(())
        }

        fn remove_deleted(&self) -> Result<(), Error> {
            self.failure()?;
            if let Some(error) = self.fail_rewrite.lock().unwrap().take() {
                return Err(error);
            }
            self.rewrites.fetch_add(1, Ordering::SeqCst);
            if !*self.broken_rewrite.lock().unwrap() {
                self.rows.lock().unwrap().retain(|r| !r.marked);
            }
            Ok(())
        }
    }

    fn row(table: &'static str, user: Option<&'static str>, anon: Option<&'static str>) -> Row {
        Row {
            table,
            site: 1,
            user,
            anon,
            marked: false,
        }
    }

    /// u1 with devices a1 and a2, u2 with a3, and the rows they leave.
    fn two_users() -> Arc<Fake> {
        Fake::with(&[
            row("events", None, Some("a1")),
            row("events", None, Some("a2")),
            row("events", None, Some("a3")),
            row("identity_alias", Some("u1"), Some("a1")),
            row("identity_alias", Some("u1"), Some("a2")),
            row("identity_alias", Some("u2"), Some("a3")),
            row("user_traits", Some("u1"), None),
            row("user_traits", Some("u2"), None),
        ])
    }

    const TIMING: Timing = Timing {
        grace: Duration::from_secs(7),
        finish_after: Duration::from_secs(100),
        max_attempts: 3,
    };

    struct World {
        meta: Arc<SqliteMeta>,
        fake: Arc<Fake>,
        job: Job,
        stats: Arc<JobStats>,
        now: Arc<AtomicI64>,
        site: u32,
    }

    /// Another server on the same metadata store and the same ClickHouse.
    fn second_server(world: &World) -> (Job, Arc<JobStats>) {
        let stats = Arc::new(JobStats::default());
        let job = Job::new(
            Arc::clone(&world.meta) as Arc<dyn MetaStore>,
            Arc::clone(&world.fake) as Arc<dyn Eraser>,
            "second",
            Arc::clone(&stats),
        );
        (job, stats)
    }

    fn world(fake: Arc<Fake>) -> World {
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = Arc::clone(&now);
        let meta = Arc::new(
            SqliteMeta::open_in_memory()
                .unwrap()
                .with_clock(move || clock.load(Ordering::SeqCst)),
        );
        let organization = meta.create_organization("o").unwrap();
        let site = meta.create_site(organization.id, "s").unwrap().id;
        let stats = Arc::new(JobStats::default());
        let job = Job::new(
            Arc::clone(&meta) as Arc<dyn MetaStore>,
            Arc::clone(&fake) as Arc<dyn Eraser>,
            "test",
            Arc::clone(&stats),
        );
        World {
            meta,
            fake,
            job,
            stats,
            now,
            site,
        }
    }

    fn now(world: &World) -> i64 {
        world.now.load(Ordering::SeqCst)
    }

    fn hide(world: &World) -> bool {
        world.job.hide_next(TIMING, now(world), &|_| true)
    }

    #[test]
    fn a_request_is_hidden_then_completed_and_only_that_user_goes() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();

        let waited = Mutex::new(Vec::new());
        assert!(w.job.hide_next(TIMING, now(&w), &|d| {
            waited.lock().unwrap().push(d);
            true
        }));
        assert_eq!(*waited.lock().unwrap(), vec![Duration::from_secs(7)]);
        let hidden = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(hidden.state, ErasureState::Hidden);
        assert_eq!(hidden.anon_count, 2);
        assert!(
            hidden.detail.as_deref().unwrap().contains("events 2"),
            "{hidden:?}"
        );
        assert!(
            tombstones_of(&w.meta).anon_erased(w.site, "a1"),
            "the devices are blocked"
        );
        assert!(!hide(&w), "nothing else to hide");

        // Not yet old enough to be rewritten.
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 0);
        assert_eq!(w.fake.rewrites.load(Ordering::SeqCst), 0);

        w.now.store(1_000 + 100, Ordering::SeqCst);
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 1);
        let done = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(done.state, ErasureState::Complete);
        assert_eq!(done.user_id, None);
        assert!(done.detail.unwrap().starts_with("verified"));
        assert!(w.meta.erasure_anons(request.id).unwrap().is_empty());
        assert_eq!(w.fake.total(), 3, "the event, link and traits of u2");
    }

    #[test]
    fn requests_that_are_due_together_share_one_rewrite() {
        let w = world(two_users());
        for user in ["u1", "u2"] {
            w.meta.request_erasure(w.site, user, "ops").unwrap();
        }
        assert!(hide(&w) && hide(&w) && !hide(&w));
        w.now.store(5_000, Ordering::SeqCst);
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 2);
        assert_eq!(w.fake.rewrites.load(Ordering::SeqCst), 1);
        assert_eq!(w.fake.total(), 0);
    }

    #[test]
    fn rows_a_slow_node_inserted_after_the_first_delete_go_at_the_end() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        assert!(hide(&w));
        w.fake.insert(row("events", None, Some("a1")));
        w.now.store(5_000, Ordering::SeqCst);
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 1);
        assert_eq!(
            w.meta.erasure_request(request.id).unwrap().state,
            ErasureState::Complete
        );
        assert_eq!(w.fake.total(), 3);
    }

    #[test]
    fn a_failure_retrying_could_fix_leaves_the_request_for_a_later_try() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        w.fake.fail_next(Error::Io("clickhouse is down".to_owned()));
        assert!(hide(&w));
        let waiting = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(waiting.state, ErasureState::Requested, "not failed");
        assert!(!hide(&w), "held by its lease");

        w.now.store(1_000 + HIDE_LEASE_SECS + 1, Ordering::SeqCst);
        assert!(hide(&w));
        assert_eq!(
            w.meta.erasure_request(request.id).unwrap().state,
            ErasureState::Hidden
        );
    }

    #[test]
    fn a_request_clickhouse_rejects_for_good_fails_with_the_reason() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        w.fake.fail_next(Error::Server {
            status: 400,
            message: "bad statement".to_owned(),
        });
        assert!(hide(&w));
        let failed = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(failed.state, ErasureState::Failed);
        assert!(failed.detail.unwrap().contains("bad statement"));
        // The operator can try again.
        w.meta.retry_erasure(request.id).unwrap();
        assert!(hide(&w));
        assert_eq!(
            w.meta.erasure_request(request.id).unwrap().state,
            ErasureState::Hidden
        );
    }

    #[test]
    fn rows_that_survive_the_rewrite_fail_the_request_instead_of_completing_it() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        assert!(hide(&w));
        *w.fake.broken_rewrite.lock().unwrap() = true;
        w.now.store(5_000, Ordering::SeqCst);
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 0);
        let failed = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(failed.state, ErasureState::Failed);
        assert!(failed.detail.unwrap().contains("rows remain"));
        assert_eq!(failed.user_id.as_deref(), Some("u1"), "kept for the retry");
    }

    #[test]
    fn stopping_during_the_wait_deletes_nothing_and_a_restart_finishes_the_job() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        assert!(w.job.hide_next(TIMING, now(&w), &|_| false));
        assert_eq!(w.fake.deletes.load(Ordering::SeqCst), 0);
        let held = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(held.state, ErasureState::Requested);
        assert_eq!(held.anon_count, 2, "the devices are already blocked");

        w.now.store(1_000 + HIDE_LEASE_SECS + 1, Ordering::SeqCst);
        assert!(hide(&w));
        assert_eq!(
            w.meta.erasure_request(request.id).unwrap().state,
            ErasureState::Hidden
        );
    }

    #[test]
    fn a_retry_after_the_links_were_already_deleted_still_knows_the_devices() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        // The first attempt tombstoned and deleted, then died before moving on.
        let devices = ["a1".to_owned(), "a2".to_owned()];
        w.meta.add_erasure_anons(request.id, &devices).unwrap();
        w.fake.delete(1, "u1", &devices).unwrap();
        w.fake.rows.lock().unwrap().retain(|r| !r.marked);
        w.fake.insert(row("events", None, Some("a2")));
        assert!(hide(&w));
        w.now.store(5_000, Ordering::SeqCst);
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 1);
        assert_eq!(w.fake.total(), 3, "the late row of a2 went too");
    }

    #[test]
    fn a_user_with_no_data_completes_without_trouble() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "nobody", "ops").unwrap();
        assert!(hide(&w));
        w.now.store(5_000, Ordering::SeqCst);
        assert_eq!(w.job.finish_due(TIMING, now(&w)), 1);
        assert_eq!(
            w.meta.erasure_request(request.id).unwrap().state,
            ErasureState::Complete
        );
        assert_eq!(w.fake.total(), 8);
    }

    fn round(job: &Job, world: &World) {
        job.run_round(TIMING, now(world), &|_| true);
    }

    #[test]
    fn only_one_server_at_a_time_does_the_work() {
        let w = world(two_users());
        let (other, other_stats) = second_server(&w);
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();

        // The first to ask leads.
        round(&w.job, &w);
        round(&other, &w);
        assert!(w.stats.leader.load(Ordering::Relaxed));
        assert!(!other_stats.leader.load(Ordering::Relaxed));
        assert_eq!(w.stats.hidden.load(Ordering::Relaxed), 1);
        assert_eq!(other_stats.hidden.load(Ordering::Relaxed), 0);
        assert_eq!(
            w.meta.erasure_request(request.id).unwrap().state,
            ErasureState::Hidden
        );

        // The other one does nothing however many times it tries, and the
        // leader keeps the lease by asking again.
        w.now.store(1_020, Ordering::SeqCst);
        round(&w.job, &w);
        w.now.store(1_040, Ordering::SeqCst);
        round(&other, &w);
        assert!(
            !other_stats.leader.load(Ordering::Relaxed),
            "renewed at 1020"
        );
    }

    #[test]
    fn another_server_takes_over_when_the_leader_goes_silent_or_steps_down() {
        let w = world(two_users());
        let (other, other_stats) = second_server(&w);
        round(&w.job, &w);
        assert!(w.stats.leader.load(Ordering::Relaxed));

        // The leader dies: after the lease the other one leads.
        w.now.store(1_000 + LEADER_LEASE_SECS, Ordering::SeqCst);
        round(&other, &w);
        assert!(other_stats.leader.load(Ordering::Relaxed));
        round(&w.job, &w);
        assert!(!w.stats.leader.load(Ordering::Relaxed), "now it stands by");

        // A server that stops lets go at once.
        other.step_down();
        assert!(!other_stats.leader.load(Ordering::Relaxed));
        round(&w.job, &w);
        assert!(w.stats.leader.load(Ordering::Relaxed));
    }

    #[test]
    fn trouble_waits_longer_each_time_and_after_the_limit_the_request_fails() {
        let w = world(two_users());
        let request = w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        let down = || Error::Io("clickhouse is down".to_owned());

        w.fake.fail_next(down());
        round(&w.job, &w);
        let first = w.meta.erasure_request(request.id).unwrap();
        assert_eq!((first.attempts, first.retry_after), (1, Some(1_030)));

        // Not tried before its time.
        w.fake.fail_next(down());
        round(&w.job, &w);
        assert_eq!(w.meta.erasure_request(request.id).unwrap().attempts, 1);
        w.fake.failure().ok();

        w.now.store(1_030, Ordering::SeqCst);
        w.fake.fail_next(down());
        round(&w.job, &w);
        let second = w.meta.erasure_request(request.id).unwrap();
        assert_eq!((second.attempts, second.retry_after), (2, Some(1_090)));

        w.now.store(1_090, Ordering::SeqCst);
        w.fake.fail_next(down());
        round(&w.job, &w);
        let last = w.meta.erasure_request(request.id).unwrap();
        assert_eq!(last.state, ErasureState::Failed);
        assert!(last.detail.unwrap().contains("gave up after 3 attempts"));
        assert_eq!(w.stats.trouble.load(Ordering::Relaxed), 3);
        assert_eq!(w.stats.gave_up.load(Ordering::Relaxed), 1);
        assert_eq!(w.stats.failed.load(Ordering::Relaxed), 1);
        round(&w.job, &w);
        assert_eq!(w.stats.summary().failed, 1, "counted at the next round");
    }

    #[test]
    fn the_wait_doubles_up_to_a_cap() {
        let waits: Vec<i64> = (1..=10).map(backoff).collect();
        assert_eq!(&waits[..4], &[30, 60, 120, 240]);
        assert!(waits.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(backoff(10), RETRY_CAP_SECS);
        assert_eq!(backoff(u32::MAX), RETRY_CAP_SECS, "no overflow");
        assert_eq!(backoff(0), 30);
    }

    #[test]
    fn a_rewrite_that_keeps_failing_fails_every_request_waiting_for_it() {
        let w = world(two_users());
        for user in ["u1", "u2"] {
            w.meta.request_erasure(w.site, user, "ops").unwrap();
        }
        round(&w.job, &w);
        assert_eq!(w.stats.hidden.load(Ordering::Relaxed), 2);
        let mut at = 5_000;
        for _ in 0..3 {
            w.now.store(at, Ordering::SeqCst);
            // Only the rewrite fails; the deletes before it work.
            *w.fake.fail_rewrite.lock().unwrap() = Some(Error::Io("timeout".to_owned()));
            round(&w.job, &w);
            at += 4_000;
        }
        let states: Vec<_> = w
            .meta
            .erasure_requests(None)
            .unwrap()
            .iter()
            .map(|r| r.state)
            .collect();
        assert_eq!(states, vec![ErasureState::Failed, ErasureState::Failed]);
        assert_eq!(w.stats.gave_up.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn the_summary_follows_the_requests_for_the_metrics() {
        let w = world(two_users());
        w.meta.request_erasure(w.site, "u1", "ops").unwrap();
        w.meta.request_erasure(w.site, "u2", "ops").unwrap();
        round(&w.job, &w);
        assert_eq!(w.stats.summary().requested, 2, "counted before the work");
        round(&w.job, &w);
        let summary = w.stats.summary();
        assert_eq!((summary.requested, summary.hidden), (0, 2));
        assert_eq!(summary.oldest_open_at, Some(1_000));
    }

    #[test]
    fn a_server_that_loses_the_lease_mid_round_starts_nothing_more() {
        let w = world(two_users());
        let job = Job::new(
            Arc::clone(&w.meta) as Arc<dyn MetaStore>,
            Arc::clone(&w.fake) as Arc<dyn Eraser>,
            "slow",
            Arc::clone(&w.stats),
        )
        .with_heartbeat(Duration::from_millis(20));
        let (other, other_stats) = second_server(&w);
        let first = w.meta.request_erasure(w.site, "u1", "ops").unwrap();

        let second = std::cell::Cell::new(None);
        // Far enough ahead that a request made during the round is due.
        job.run_round(TIMING, 10_000, &|_| {
            if second.get().is_none() {
                // The server stalls past its lease and another takes it...
                w.now.store(1_000 + LEADER_LEASE_SECS, Ordering::SeqCst);
                round(&other, &w);
                // ...then a request arrives that only the old leader could
                // still pick up in this round.
                second.set(Some(
                    w.meta.request_erasure(w.site, "u2", "ops").unwrap().id,
                ));
                std::thread::sleep(Duration::from_millis(300));
            }
            true
        });
        assert!(other_stats.leader.load(Ordering::Relaxed));
        assert!(!w.stats.leader.load(Ordering::Relaxed), "it knows it lost");
        assert_eq!(
            w.meta.erasure_request(first.id).unwrap().state,
            ErasureState::Hidden,
            "the one in progress was finished"
        );
        assert_eq!(
            w.meta.erasure_request(second.get().unwrap()).unwrap().state,
            ErasureState::Requested,
            "and nothing was started after the lease was lost"
        );
        assert_eq!(w.stats.hidden.load(Ordering::Relaxed), 1);
    }
}
