use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use dionaea_ingest::{Batcher, BatcherConfig, Cache, Destination};
use dionaea_meta::SqliteMeta;
use dionaea_spool::{Config as SpoolConfig, Spool};
use dionaea_store::Store;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::collect::{self, Collect, Settings};
use crate::config::{Config, clickhouse_endpoint};

/// How often a node asks the metadata store whether keys or schemas changed.
const REFRESH_EVERY: Duration = Duration::from_secs(2);
/// The longest wait between attempts to prepare ClickHouse.
const MIGRATE_BACKOFF_MAX: Duration = Duration::from_secs(30);

pub(crate) fn router(collect: Option<Arc<Collect>>) -> Router {
    let router = Router::new().route("/healthz", get(healthz));
    match collect {
        Some(collect) => router.merge(collect::routes(collect)),
        None => router,
    }
}

async fn healthz() -> &'static str {
    "ok"
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
    fn start(config: &Config) -> Result<(Self, Arc<Collect>), String> {
        let (host, port) =
            clickhouse_endpoint(&config.clickhouse_url).map_err(|reason| reason.to_string())?;
        let meta = SqliteMeta::open(&config.metadata_path)
            .map_err(|e| format!("cannot open {}: {e}", config.metadata_path.display()))?;
        let cache = Cache::new();
        cache
            .refresh(&meta)
            .map_err(|e| format!("cannot read the metadata: {e}"))?;

        let mut spool_config = SpoolConfig::new(&config.spool_dir);
        spool_config.quota_bytes = config.spool_quota_bytes;
        let spool = Spool::open(spool_config)
            .map_err(|e| format!("cannot open the spool {}: {e}", config.spool_dir.display()))?;

        let mut store_config = dionaea_store::Config::new(host, port, &config.clickhouse_database);
        store_config.user.clone_from(&config.clickhouse_user);
        store_config
            .password
            .clone_from(&config.clickhouse_password.0);
        let store = Store::new(store_config).map_err(|e| format!("clickhouse settings: {e}"))?;

        let collect = Arc::new(Collect::new(
            cache,
            spool.clone(),
            Settings::default(),
            collect::system_clock,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let batcher = Arc::new(Mutex::new(None));
        let mut threads = Vec::new();

        let refresher = {
            let (collect, stop) = (Arc::clone(&collect), Arc::clone(&stop));
            thread::Builder::new()
                .name("dionaea-refresh".to_owned())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        nap(REFRESH_EVERY, &stop);
                        match collect.cache.refresh(&meta) {
                            Ok(true) => info!("keys and schemas reloaded"),
                            Ok(false) => {}
                            Err(error) => warn!(%error, "cannot refresh keys and schemas"),
                        }
                    }
                })
                .map_err(|e| format!("cannot start the refresh thread: {e}"))?
        };
        threads.push(refresher);

        // The tables must exist before anything is inserted: a missing table
        // is an error the batcher would keep retrying, so it starts only once
        // ClickHouse is prepared. Until then requests are still accepted into
        // the spool.
        let bootstrap = {
            let (stop, slot) = (Arc::clone(&stop), Arc::clone(&batcher));
            thread::Builder::new()
                .name("dionaea-bootstrap".to_owned())
                .spawn(move || {
                    let mut wait = Duration::from_millis(500);
                    while !stop.load(Ordering::Acquire) {
                        match store.migrate() {
                            Ok(applied) => {
                                info!(applied = applied.len(), "clickhouse is ready");
                                let destination: Arc<dyn Destination> = Arc::new(store);
                                match Batcher::start(spool, destination, BatcherConfig::default()) {
                                    Ok(batcher) => {
                                        *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                                            Some(batcher);
                                    }
                                    Err(error) => {
                                        warn!(%error, "cannot start the batcher");
                                    }
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
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.listen))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;

    info!(listen = %local, "listening");
    let config_for_node = config.clone();
    let (node, collect) = tokio::task::spawn_blocking(move || Node::start(&config_for_node))
        .await
        .map_err(|e| e.to_string())??;

    let served = axum::serve(
        listener,
        router(Some(collect)).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    let _ = tokio::task::spawn_blocking(move || node.stop()).await;
    served.map_err(|e| e.to_string())?;
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
