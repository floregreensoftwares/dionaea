use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use dionaea_ingest::{Batcher, BatcherConfig, Cache, Destination, MaxMind};
use dionaea_spool::{Config as SpoolConfig, Spool};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::collect::{self, Collect, Settings};
use crate::config::Config;
use crate::erasure::{Eraser, Job, Timing};
use crate::net::{self, NetControl, NetCounters};
use crate::ops::{self, Ops, Status};
use crate::retention::{Expirer, Job as RetentionJob};
use crate::settings::{self, Origin, Tuning};

/// The longest wait between attempts to prepare ClickHouse.
const MIGRATE_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// The largest spool record: a valid request's rows always fit one (see
/// `dionaea_ingest::CHUNK_BYTES`).
const SPOOL_RECORD_BYTES: u32 = 4 * 1024 * 1024;
/// How long one erasure statement may run: rewriting the tables of a large
/// installation takes far longer than an insert.
const ERASE_TIMEOUT: Duration = Duration::from_secs(1800);
/// How often the retention job looks at whether a run is due.
const RETENTION_TICK: Duration = Duration::from_secs(30);
/// How often the erasure job looks for work.
const ERASE_TICK: Duration = Duration::from_secs(5);

/// The public listener's routes: the collect endpoint, and `/healthz` for
/// load balancers that probe the port events arrive on.
pub(crate) fn router(collect: Option<Arc<Collect>>) -> Router {
    let router = Router::new().route("/healthz", get(|| async { "ok" }));
    match collect {
        Some(collect) => router.merge(collect::routes(collect)),
        None => router,
    }
}

/// Sleeps `total` in short steps, returning early when `stop` is set.
fn nap(total: Duration, stop: &AtomicBool) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Acquire) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// The parts of the ingest path that run beside the HTTP server: the cache
/// refresher, and the thread that prepares ClickHouse and then drains the
/// spool into it.
struct Node {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    batcher: Arc<Mutex<Option<Batcher>>>,
}

/// The settings in force and how to put new ones into force: the pieces the
/// refresher changes in place when an operator stores a setting.
#[derive(Clone)]
struct Live {
    /// Defaults, file and environment: what stored settings are applied on.
    base: Arc<Tuning>,
    /// Where each of those values came from, to say what a stored one overrides.
    origins: Arc<BTreeMap<&'static str, Origin>>,
    current: Arc<RwLock<Arc<Tuning>>>,
    collect: Arc<Collect>,
    control: Arc<NetControl>,
    batcher: Arc<Mutex<Option<Batcher>>>,
    status: Arc<Status>,
    /// What was last reported in the log, so a standing condition is said once.
    reported: Arc<Mutex<Vec<String>>>,
}

fn batcher_config(tuning: &Tuning) -> BatcherConfig {
    BatcherConfig {
        max_records: tuning.batch_max_records,
        max_bytes: tuning.batch_max_bytes,
        min_interval: tuning.batch_min_interval,
        shared_dedup: tuning.shared_dedup,
        ..BatcherConfig::default()
    }
}

impl Live {
    fn tuning(&self) -> Arc<Tuning> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Works out the settings in force at `now_ms` from what the cache holds
    /// and puts them into force if they differ from the ones in force. Returns
    /// what changed, as `name: before -> after`, and when the next stored
    /// change is due.
    ///
    /// The result depends only on the stored values and the time, so servers
    /// that read the same store switch at the same moment.
    fn sync(&self, now_ms: i64) -> (Vec<String>, Option<i64>) {
        let stored = self.collect.cache.settings();
        let due = settings::next_change(&stored, now_ms);
        let resolved = settings::resolve_at(&stored, now_ms);
        let (next, skipped) = self.base.with_stored(&resolved);

        self.status
            .settings_skipped
            .store(skipped.len() as u64, Ordering::Relaxed);
        self.status
            .settings_stored
            .store(resolved.len() as u64, Ordering::Relaxed);
        self.report(&resolved, &skipped);

        let before = self.tuning();
        if *before == next {
            return (Vec::new(), due);
        }
        let changed: Vec<String> = settings::SPECS
            .iter()
            .filter(|spec| before.show(spec) != next.show(spec))
            .map(|spec| {
                format!(
                    "{}: {} -> {}",
                    spec.name,
                    before.show(spec),
                    next.show(spec)
                )
            })
            .collect();
        self.put(next);
        (changed, due)
    }

    /// Says in the log which stored values are ignored and which override the
    /// file or the environment, once for as long as that stays true.
    fn report(&self, resolved: &BTreeMap<String, String>, skipped: &[(String, String)]) {
        let mut lines: Vec<String> = skipped
            .iter()
            .map(|(name, reason)| format!("ignored {name}: {reason}"))
            .collect();
        lines.extend(
            settings::overridden(&self.base, &self.origins, resolved)
                .into_iter()
                .map(|(name, from, was)| {
                    format!("{name}: a stored value overrides {} ({was})", from.as_str())
                }),
        );
        let mut reported = self.reported.lock().unwrap_or_else(PoisonError::into_inner);
        if *reported != lines {
            for line in lines.iter().filter(|line| !reported.contains(line)) {
                warn!("{line}");
            }
            *reported = lines;
        }
    }

    /// Puts `tuning` into force everywhere.
    fn put(&self, tuning: Tuning) {
        // The shared copy first: a batcher that starts at this moment reads it.
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(tuning.clone());
        self.collect.apply(Settings::from_tuning(&tuning));
        self.control
            .set(tuning.max_connections, tuning.header_timeout);
        if let Some(batcher) = self
            .batcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            batcher.reconfigure(batcher_config(&tuning));
        }
    }
}

/// A name for this server in the metadata store's leases: the process id, the
/// start time and 64 random bits, so two servers do not share one even on
/// different machines.
fn worker_name() -> String {
    use std::hash::{BuildHasher, Hasher};

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // The standard library seeds every `RandomState` from the system.
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{}-{started:x}-{random:x}", std::process::id())
}

/// The time now, in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    i64::try_from(collect::system_clock()).unwrap_or(i64::MAX / 2)
}

impl Node {
    fn start(
        config: &Config,
        status: &Arc<Status>,
        control: &Arc<NetControl>,
    ) -> Result<(Self, Arc<Collect>), String> {
        let meta = config.open_meta()?;
        let cache = Cache::new();
        cache
            .refresh(&meta)
            .map_err(|e| format!("cannot read the metadata: {e}"))?;

        // The settings an operator stored apply from the first request.
        let resolved = settings::resolve_at(&cache.settings(), now_ms());
        let (tuning, skipped) = config.tuning.with_stored(&resolved);
        status
            .settings_stored
            .store(resolved.len() as u64, Ordering::Relaxed);
        status
            .settings_skipped
            .store(skipped.len() as u64, Ordering::Relaxed);
        control.set(tuning.max_connections, tuning.header_timeout);

        let mut spool_config = SpoolConfig::new(&config.spool_dir);
        spool_config.quota_bytes = config.spool_quota_bytes;
        spool_config.max_record_bytes = SPOOL_RECORD_BYTES;
        let spool = Spool::open(spool_config)
            .map_err(|e| format!("cannot open the spool {}: {e}", config.spool_dir.display()))?;

        let eraser = config.clickhouse_store(Some(ERASE_TIMEOUT))?;
        // Removing old data is as slow as erasing: the same long timeout.
        let expirer = eraser.clone();
        let store = config.clickhouse_store(None)?;

        // The user agent dataset compiles its patterns on first use; do it now
        // so the first request does not pay for it.
        let _ = dionaea_ingest::parse_user_agent("Mozilla/5.0 (X11; Linux x86_64) warm-up");

        let mut collect = Collect::new(
            cache,
            spool.clone(),
            Settings::from_tuning(&tuning),
            collect::system_clock,
        );
        if let Some(path) = &config.geoip_path {
            let lookup = MaxMind::open(path)?;
            collect = collect.with_country_lookup(Arc::new(lookup));
            info!("country database loaded");
        }
        let collect = collect
            .with_dedup_journal(&config.spool_dir.join("dedup.journal"))
            .map_err(|e| format!("cannot open the event id journal: {e}"))?;
        let collect = Arc::new(collect);
        let stop = Arc::new(AtomicBool::new(false));
        let batcher = Arc::new(Mutex::new(None));
        let live = Live {
            base: Arc::new(config.tuning.clone()),
            origins: Arc::new(config.tuning_origins.clone()),
            reported: Arc::new(Mutex::new(Vec::new())),
            current: Arc::new(RwLock::new(Arc::new(tuning))),
            collect: Arc::clone(&collect),
            control: Arc::clone(control),
            batcher: Arc::clone(&batcher),
            status: Arc::clone(status),
        };
        let mut threads = Vec::new();

        let refresher = {
            let (live, stop, status) = (live.clone(), Arc::clone(&stop), Arc::clone(status));
            thread::Builder::new()
                .name("dionaea-refresh".to_owned())
                .spawn(move || {
                    // The journal is made durable every second whatever the
                    // refresh interval is, so a crash loses at most a second of
                    // remembered ids; drop counts are written on their own timer.
                    // Settings are worked out from the clock on every tick, and
                    // the tick is short while a stored change is waiting for its
                    // moment, so servers switch within a fraction of a second.
                    let mut last_refresh = Instant::now();
                    let mut last_flush = Instant::now();
                    let mut due = None::<i64>;
                    while !stop.load(Ordering::Acquire) {
                        let every = live.tuning().refresh_every;
                        let tick = if due.is_some() {
                            Duration::from_millis(100)
                        } else {
                            every.min(Duration::from_secs(1))
                        };
                        nap(tick, &stop);
                        let collect = &live.collect;
                        if let Err(error) = collect.sync_dedup() {
                            warn!(%error, "cannot write the event id journal");
                        }
                        if collect.drops_full()
                            || last_flush.elapsed() >= collect.drop_flush_every()
                        {
                            last_flush = Instant::now();
                            collect.flush_drops();
                        }
                        if last_refresh.elapsed() >= every {
                            last_refresh = Instant::now();
                            match collect.cache.refresh(&meta) {
                                Ok(reloaded) => {
                                    status.metadata_failing.store(false, Ordering::Relaxed);
                                    if reloaded {
                                        info!("keys, schemas and settings reloaded");
                                    }
                                }
                                Err(error) => {
                                    status.metadata_failing.store(true, Ordering::Relaxed);
                                    warn!(%error, "cannot refresh keys and schemas");
                                }
                            }
                        }
                        let (changes, next) = live.sync(now_ms());
                        due = next;
                        for change in changes {
                            info!(%change, "a setting changed");
                        }
                    }
                })
                .map_err(|e| format!("cannot start the refresh thread: {e}"))?
        };
        threads.push(refresher);

        // Erasure requests are carried out once ClickHouse is prepared. The
        // job has its own connection to the metadata store and its own
        // ClickHouse handle with a long timeout.
        let erasure = {
            let job_meta = config.open_meta()?;
            let (live, stop, status) = (live.clone(), Arc::clone(&stop), Arc::clone(status));
            let job = Job::new(
                Arc::new(job_meta),
                Arc::new(eraser) as Arc<dyn Eraser>,
                worker_name(),
                Arc::clone(&status.erasure),
            );
            thread::Builder::new()
                .name("dionaea-erasure".to_owned())
                .spawn(move || {
                    let secs = |at: i64| at / 1000;
                    while !stop.load(Ordering::Acquire) {
                        nap(ERASE_TICK, &stop);
                        if !status.clickhouse_ready.load(Ordering::Relaxed) {
                            continue;
                        }
                        let tuning = live.tuning();
                        // Every node must have reloaded the tombstones before
                        // anything is deleted: at least two refreshes.
                        let timing = Timing {
                            grace: tuning.erase_grace.max(tuning.refresh_every * 2),
                            finish_after: tuning.erase_finish_after,
                            max_attempts: tuning.erase_max_attempts,
                            failed_keep: tuning.erase_failed_keep,
                        };
                        let wait = |grace: Duration| {
                            nap(grace, &stop);
                            !stop.load(Ordering::Acquire)
                        };
                        job.run_round(timing, secs(now_ms()), &wait);
                    }
                    job.step_down();
                })
                .map_err(|e| format!("cannot start the erasure thread: {e}"))?
        };
        threads.push(erasure);

        // Old data is removed once per interval by whichever server finds the
        // run unclaimed, once ClickHouse is prepared.
        let retention = {
            let meta = config.open_meta()?;
            let (live, stop, status) = (live.clone(), Arc::clone(&stop), Arc::clone(status));
            let job = RetentionJob::new(
                Arc::new(meta),
                Arc::new(expirer) as Arc<dyn Expirer>,
                Arc::clone(&status.retention),
            );
            thread::Builder::new()
                .name("dionaea-retention".to_owned())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        nap(RETENTION_TICK, &stop);
                        if !status.clickhouse_ready.load(Ordering::Relaxed) {
                            continue;
                        }
                        let tuning = live.tuning();
                        job.run_if_due(
                            tuning.retention_interval,
                            tuning.retention_default_days,
                            collect::system_clock(),
                        );
                    }
                })
                .map_err(|e| format!("cannot start the retention thread: {e}"))?
        };
        threads.push(retention);

        // The tables must exist before anything is inserted: a missing table
        // is an error the batcher would keep retrying, so it starts only once
        // ClickHouse is prepared. Until then requests are still accepted into
        // the spool.
        let bootstrap = {
            let (stop, slot, status, live) = (
                Arc::clone(&stop),
                Arc::clone(&batcher),
                Arc::clone(status),
                live.clone(),
            );
            thread::Builder::new()
                .name("dionaea-bootstrap".to_owned())
                .spawn(move || {
                    let mut wait = Duration::from_millis(500);
                    while !stop.load(Ordering::Acquire) {
                        match store.migrate() {
                            Ok(applied) => {
                                info!(applied = applied.len(), "clickhouse is ready");
                                let destination: Arc<dyn Destination> = Arc::new(store);
                                let config = batcher_config(&live.tuning());
                                // Read at every batch, so an erasure takes hold
                                // as soon as this node has reloaded the metadata.
                                let erased = {
                                    let collect = Arc::clone(&live.collect);
                                    Arc::new(move || collect.cache.tombstones())
                                };
                                match Batcher::start_with(spool, destination, config, erased) {
                                    Ok(started) => {
                                        *status
                                            .batcher
                                            .lock()
                                            .unwrap_or_else(PoisonError::into_inner) =
                                            Some(started.stats_handle());
                                        status.clickhouse_ready.store(true, Ordering::Relaxed);
                                        let mut slot =
                                            slot.lock().unwrap_or_else(PoisonError::into_inner);
                                        // A setting stored while it was starting.
                                        started.reconfigure(batcher_config(&live.tuning()));
                                        *slot = Some(started);
                                    }
                                    Err(error) => warn!(%error, "cannot start the batcher"),
                                }
                                return;
                            }
                            Err(error) => {
                                warn!(%error, "clickhouse is not ready; events stay in the spool");
                                nap(wait, &stop);
                                wait = (wait * 2).min(MIGRATE_BACKOFF_MAX);
                            }
                        }
                    }
                })
                .map_err(|e| format!("cannot start the bootstrap thread: {e}"))?
        };
        threads.push(bootstrap);

        Ok((
            Self {
                stop,
                threads,
                batcher,
            },
            collect,
        ))
    }

    /// Stops the threads. What the batcher has not confirmed stays in the
    /// spool for the next start.
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads {
            let _ = thread.join();
        }
        let batcher = self
            .batcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(batcher) = batcher {
            batcher.stop();
        }
    }
}

/// Serves until `shutdown` resolves, then stops accepting and drains
/// in-flight requests.
pub(crate) async fn run(
    config: &Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let bind = |address| async move {
        TcpListener::bind(address)
            .await
            .map_err(|e| format!("cannot listen on {address}: {e}"))
    };
    let public = bind(config.listen).await?;
    let operator = bind(config.ops_listen).await?;
    let (public_address, ops_address) = (
        public.local_addr().map_err(|e| e.to_string())?,
        operator.local_addr().map_err(|e| e.to_string())?,
    );
    info!(listen = %public_address, ops = %ops_address, "listening");

    let status = Arc::new(Status::new(config.spool_quota_bytes));
    let control = Arc::new(NetControl::new(
        config.tuning.max_connections,
        config.tuning.header_timeout,
    ));
    let (node, collect) = {
        let (config, status, control) = (config.clone(), Arc::clone(&status), Arc::clone(&control));
        tokio::task::spawn_blocking(move || Node::start(&config, &status, &control))
            .await
            .map_err(|e| e.to_string())??
    };

    let public_counters = Arc::new(NetCounters::default());
    let collect_for_stop = Arc::clone(&collect);
    let ops_state = Arc::new(Ops {
        collect: Arc::clone(&collect),
        status,
        public: Arc::clone(&public_counters),
    });

    // One shutdown signal for both listeners.
    let (signal, mut stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = signal.send(true);
    });
    let wait_public = {
        let mut stopped = stopped.clone();
        async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        }
    };
    let wait_ops = async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    };

    // The operator listener is small and local: a few connections, short
    // header time.
    let ops_control = Arc::new(NetControl::new(64, Duration::from_secs(10)));
    tokio::join!(
        net::serve(
            public,
            router(Some(collect)),
            control,
            Arc::clone(&public_counters),
            wait_public
        ),
        net::serve(
            operator,
            ops::routes(ops_state),
            ops_control,
            Arc::new(NetCounters::default()),
            wait_ops
        ),
    );
    {
        let collect = Arc::clone(&collect_for_stop);
        let _ = tokio::task::spawn_blocking(move || collect.flush_drops()).await;
    }
    let _ = tokio::task::spawn_blocking(move || node.stop()).await;
    info!("shutdown complete");
    Ok(())
}

/// Registers the handlers now and returns a future that resolves on Ctrl+C or,
/// on Unix, SIGTERM. Registering before the server says it is listening means a
/// signal sent right after cannot reach the process while its default action,
/// ending it at once, is still in place. Call it inside the runtime.
// Not an `async fn` on purpose: the handlers must be registered when it is
// called, not when it is first polled (on Windows there is nothing to register).
#[allow(clippy::manual_async_fn)]
pub(crate) fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    #[cfg(unix)]
    let (mut interrupt, mut terminate) = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::interrupt()).ok(),
            signal(SignalKind::terminate()).ok(),
        )
    };
    async move {
        #[cfg(unix)]
        {
            async fn wait(signal: &mut Option<tokio::signal::unix::Signal>) {
                match signal {
                    Some(signal) => {
                        signal.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }
            tokio::select! {
                () = wait(&mut interrupt) => info!("interrupt received"),
                () = wait(&mut terminate) => info!("terminate received"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            info!("interrupt received");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_answers_ok() {
        let response = router(None)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_path_is_not_found() {
        let response = router(None)
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
