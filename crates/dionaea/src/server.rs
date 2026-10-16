use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use dionaea_ingest::{Batcher, BatcherConfig, Cache, Destination, MaxMind};
use dionaea_meta::SqliteMeta;
use dionaea_spool::{Config as SpoolConfig, Spool};
use dionaea_store::{Store, TlsConfig};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::collect::{self, Collect, Settings};
use crate::config::{Config, clickhouse_endpoint};
use crate::net::{self, NetCounters, NetLimits};
use crate::ops::{self, Ops, Status};

/// The longest wait between attempts to prepare ClickHouse.
const MIGRATE_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// The largest spool record: a valid request's rows always fit one (see
/// `dionaea_ingest::CHUNK_BYTES`).
const SPOOL_RECORD_BYTES: u32 = 4 * 1024 * 1024;

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

impl Node {
    fn start(config: &Config, status: &Arc<Status>) -> Result<(Self, Arc<Collect>), String> {
        let endpoint = clickhouse_endpoint(&config.clickhouse_url)?;
        let tuning = &config.tuning;
        let meta = SqliteMeta::open(&config.metadata_path)
            .map_err(|e| format!("cannot open {}: {e}", config.metadata_path.display()))?;
        let cache = Cache::new();
        cache
            .refresh(&meta)
            .map_err(|e| format!("cannot read the metadata: {e}"))?;

        let mut spool_config = SpoolConfig::new(&config.spool_dir);
        spool_config.quota_bytes = config.spool_quota_bytes;
        spool_config.max_record_bytes = SPOOL_RECORD_BYTES;
        let spool = Spool::open(spool_config)
            .map_err(|e| format!("cannot open the spool {}: {e}", config.spool_dir.display()))?;

        let mut store_config =
            dionaea_store::Config::new(&endpoint.host, endpoint.port, &config.clickhouse_database);
        store_config.user.clone_from(&config.clickhouse_user);
        store_config
            .password
            .clone_from(&config.clickhouse_password.0);
        if endpoint.tls {
            store_config.tls = Some(TlsConfig {
                ca_file: config.clickhouse_ca_file.clone(),
                server_name: config.clickhouse_server_name.clone(),
            });
        }
        let store = Store::new(store_config).map_err(|e| format!("clickhouse settings: {e}"))?;

        // The user agent dataset compiles its patterns on first use; do it now
        // so the first request does not pay for it.
        let _ = dionaea_ingest::parse_user_agent("Mozilla/5.0 (X11; Linux x86_64) warm-up");

        let mut collect = Collect::new(
            cache,
            spool.clone(),
            Settings::from_tuning(tuning),
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
        let mut threads = Vec::new();

        let refresher = {
            let (collect, stop, status) =
                (Arc::clone(&collect), Arc::clone(&stop), Arc::clone(status));
            let every = tuning.refresh_every;
            thread::Builder::new()
                .name("dionaea-refresh".to_owned())
                .spawn(move || {
                    // The journal is made durable every second whatever the
                    // refresh interval is, so a crash loses at most a second of
                    // remembered ids; drop counts are written on their own timer.
                    let tick = every.min(Duration::from_secs(1));
                    let mut last_refresh = Instant::now();
                    let mut last_flush = Instant::now();
                    while !stop.load(Ordering::Acquire) {
                        nap(tick, &stop);
                        if let Err(error) = collect.sync_dedup() {
                            warn!(%error, "cannot write the event id journal");
                        }
                        if collect.drops_full()
                            || last_flush.elapsed() >= collect.drop_flush_every()
                        {
                            last_flush = Instant::now();
                            collect.flush_drops();
                        }
                        if last_refresh.elapsed() < every {
                            continue;
                        }
                        last_refresh = Instant::now();
                        match collect.cache.refresh(&meta) {
                            Ok(reloaded) => {
                                status.metadata_failing.store(false, Ordering::Relaxed);
                                if reloaded {
                                    info!("keys and schemas reloaded");
                                }
                            }
                            Err(error) => {
                                status.metadata_failing.store(true, Ordering::Relaxed);
                                warn!(%error, "cannot refresh keys and schemas");
                            }
                        }
                    }
                })
                .map_err(|e| format!("cannot start the refresh thread: {e}"))?
        };
        threads.push(refresher);

        let batcher_config = BatcherConfig {
            max_records: tuning.batch_max_records,
            max_bytes: tuning.batch_max_bytes,
            min_interval: tuning.batch_min_interval,
            shared_dedup: tuning.shared_dedup,
            ..BatcherConfig::default()
        };
        // The tables must exist before anything is inserted: a missing table
        // is an error the batcher would keep retrying, so it starts only once
        // ClickHouse is prepared. Until then requests are still accepted into
        // the spool.
        let bootstrap = {
            let (stop, slot, status) =
                (Arc::clone(&stop), Arc::clone(&batcher), Arc::clone(status));
            thread::Builder::new()
                .name("dionaea-bootstrap".to_owned())
                .spawn(move || {
                    let mut wait = Duration::from_millis(500);
                    while !stop.load(Ordering::Acquire) {
                        match store.migrate() {
                            Ok(applied) => {
                                info!(applied = applied.len(), "clickhouse is ready");
                                let destination: Arc<dyn Destination> = Arc::new(store);
                                match Batcher::start(spool, destination, batcher_config) {
                                    Ok(started) => {
                                        *status
                                            .batcher
                                            .lock()
                                            .unwrap_or_else(PoisonError::into_inner) =
                                            Some(started.stats_handle());
                                        status.clickhouse_ready.store(true, Ordering::Relaxed);
                                        *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                                            Some(started);
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
    let (node, collect) = {
        let (config, status) = (config.clone(), Arc::clone(&status));
        tokio::task::spawn_blocking(move || Node::start(&config, &status))
            .await
            .map_err(|e| e.to_string())??
    };

    let limits = NetLimits {
        max_connections: config.tuning.max_connections,
        header_timeout: config.tuning.header_timeout,
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
    let ops_limits = NetLimits {
        max_connections: 64,
        header_timeout: Duration::from_secs(10),
    };
    tokio::join!(
        net::serve(
            public,
            router(Some(collect)),
            limits,
            Arc::clone(&public_counters),
            wait_public
        ),
        net::serve(
            operator,
            ops::routes(ops_state),
            ops_limits,
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

/// Resolves on Ctrl+C or, on Unix, SIGTERM.
pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("interrupt received"),
        () = terminate => info!("terminate received"),
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
