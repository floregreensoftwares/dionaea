//! The accept loop. `axum::serve` has no bound on how many connections are
//! open or how long a client may take to send its headers, so a slow or
//! numerous client could hold every socket. This loop adds both bounds and
//! hands each request to the router with the peer address attached.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tracing::warn;

/// How long open connections get to finish their request at shutdown.
const DRAIN: Duration = Duration::from_secs(10);
/// The most a connection may buffer while reading a request head. hyper's
/// default is far larger, and it is paid per connection.
const MAX_HEADER_BUFFER: usize = 32 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct NetLimits {
    /// Connections open at once; a further one is closed as soon as it opens.
    pub(crate) max_connections: usize,
    /// Time to receive the headers of a request. It also ends a keep-alive
    /// connection that stays idle.
    pub(crate) header_timeout: Duration,
}

/// Connection counts, for the metrics.
#[derive(Debug, Default)]
pub(crate) struct NetCounters {
    pub(crate) accepted: AtomicU64,
    pub(crate) refused: AtomicU64,
    open: AtomicU64,
}

impl NetCounters {
    pub(crate) fn open(&self) -> u64 {
        self.open.load(Ordering::Relaxed)
    }
}

/// Serves `app` on `listener` until `shutdown` resolves, then lets the open
/// connections finish (for a bounded time).
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    limits: NetLimits,
    counters: Arc<NetCounters>,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(connection) => connection,
                Err(error) => {
                    // Out of file descriptors and the like: wait, do not spin.
                    warn!(%error, "cannot accept a connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            counters.refused.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        counters.accepted.fetch_add(1, Ordering::Relaxed);
        counters.open.fetch_add(1, Ordering::Relaxed);
        let _ = stream.set_nodelay(true);

        let app = app.clone();
        let service = service_fn(move |mut request: hyper::Request<Incoming>| {
            request
                .extensions_mut()
                .insert(ConnectInfo::<SocketAddr>(peer));
            app.clone().oneshot(request.map(Body::new))
        });
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_timeout)
            .max_buf_size(MAX_HEADER_BUFFER)
            .keep_alive(true);
        let connection = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
        let counters = Arc::clone(&counters);
        tokio::spawn(async move {
            let _ = connection.await;
            counters.open.fetch_sub(1, Ordering::Relaxed);
            drop(slot);
        });
    }
    drop(listener);
    if tokio::time::timeout(DRAIN, graceful.shutdown())
        .await
        .is_err()
    {
        warn!("some connections were still open after the drain time");
    }
}
