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
//! A failure that retrying could fix (ClickHouse unreachable) leaves the
//! request where it is: its lease runs out and a worker takes it again. One
//! that cannot be fixed by retrying fails the request with the reason.
//!
//! Nothing here logs an identifier: requests are named by their number.

use std::sync::Arc;
use std::time::Duration;

use dionaea_meta::{ErasureRequest, ErasureState, MetaError, MetaStore};
use dionaea_store::{Error, Store, TableCount};
use tracing::{info, warn};

/// How long a worker holds a request while it hides it.
const HIDE_LEASE_SECS: i64 = 600;
/// How long a worker holds the requests it is finishing.
const FINISH_LEASE_SECS: i64 = 3600;

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
}

pub(crate) struct Job {
    meta: Arc<dyn MetaStore>,
    eraser: Arc<dyn Eraser>,
    worker: String,
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
    ) -> Self {
        Self {
            meta,
            eraser,
            worker: worker.into(),
        }
    }

    /// What to do with a step that failed: leave the request for a later try
    /// when the cause is transient, fail it when retrying cannot help.
    fn trouble(&self, request: &ErasureRequest, error: &Error) {
        if error.is_permanent() {
            warn!(request = request.id, %error, "an erasure request cannot be carried out");
            self.fail(request.id, &format!("rejected by ClickHouse: {error}"));
        } else {
            warn!(request = request.id, %error, "an erasure step failed; it will be tried again");
        }
    }

    fn fail(&self, id: u64, why: &str) {
        if let Err(error) = self
            .meta
            .advance_erasure(id, ErasureState::Failed, Some(why))
        {
            warn!(request = id, %error, "cannot mark an erasure request as failed");
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
                self.trouble(&request, &error);
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
                self.trouble(&request, &error);
                return true;
            }
        };
        let report = format!("hidden: {}; devices {}", describe(&before), anons.len());
        match self
            .meta
            .advance_erasure(request.id, ErasureState::Hidden, Some(&report))
        {
            Ok(_) => info!(request = request.id, "an erasure request is hidden"),
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
                Err(error) => self.trouble(&request, &error),
            }
        }
        if ready.is_empty() {
            return 0;
        }
        if let Err(error) = self.eraser.remove_deleted() {
            warn!(%error, "cannot remove the deleted rows; they will be tried again");
            return 0;
        }

        let mut completed = 0;
        for (request, user, anons) in ready {
            let counts = match self.eraser.count(request.site_id, &user, &anons, true) {
                Ok(counts) => counts,
                Err(error) => {
                    self.trouble(&request, &error);
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
    };

    struct World {
        meta: Arc<SqliteMeta>,
        fake: Arc<Fake>,
        job: Job,
        now: Arc<AtomicI64>,
        site: u32,
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
        let job = Job::new(
            Arc::clone(&meta) as Arc<dyn MetaStore>,
            Arc::clone(&fake) as Arc<dyn Eraser>,
            "test",
        );
        World {
            meta,
            fake,
            job,
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
            w.meta
                .snapshot()
                .unwrap()
                .tombstones
                .anon_erased(w.site, "a1"),
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
}
