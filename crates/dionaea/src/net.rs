//! The accept loop. `axum::serve` has no bound on how many connections are
//! open or how long a client may take to send its headers, so a slow or
//! numerous client could hold every socket. This loop adds both bounds and
//! hands each request to the router with the peer address attached.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
use tower::ServiceExt;
use tracing::warn;

/// How long open connections get to finish their request at shutdown.
const DRAIN: Duration = Duration::from_secs(10);
/// The most a connection may buffer while reading a request head. hyper's
/// default is far larger, and it is paid per connection.
const MAX_HEADER_BUFFER: usize = 32 * 1024;

/// The limits of a listener, which can change while it serves: a connection
/// already open keeps the header time it was given, new ones get the new one.
#[derive(Debug)]
pub(crate) struct NetControl {
    /// Connections open at once; a further one is closed as soon as it opens.
    max_connections: AtomicUsize,
    /// Time to receive the headers of a request, in milliseconds. It also ends
    /// a keep-alive connection that stays idle.
    header_timeout_ms: AtomicU64,
}

impl NetControl {
    pub(crate) fn new(max_connections: usize, header_timeout: Duration) -> Self {
        let control = Self {
            max_connections: AtomicUsize::new(0),
            header_timeout_ms: AtomicU64::new(0),
        };
        control.set(max_connections, header_timeout);
        control
    }

    pub(crate) fn set(&self, max_connections: usize, header_timeout: Duration) {
        self.max_connections
            .store(max_connections, Ordering::Relaxed);
        let millis = u64::try_from(header_timeout.as_millis()).unwrap_or(u64::MAX);
        self.header_timeout_ms.store(millis, Ordering::Relaxed);
    }

    fn header_timeout(&self) -> Duration {
        Duration::from_millis(self.header_timeout_ms.load(Ordering::Relaxed))
    }
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
    control: Arc<NetControl>,
    counters: Arc<NetCounters>,
    shutdown: impl Future<Output = ()>,
) {
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
        // Counted first, then compared: two connections arriving together
        // cannot both take the last place.
        let before = counters.open.fetch_add(1, Ordering::AcqRel);
        if before >= control.max_connections.load(Ordering::Relaxed) as u64 {
            counters.open.fetch_sub(1, Ordering::AcqRel);
            counters.refused.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        counters.accepted.fetch_add(1, Ordering::Relaxed);
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
            .header_read_timeout(control.header_timeout())
            .max_buf_size(MAX_HEADER_BUFFER)
            .keep_alive(true);
        let connection = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
        let counters = Arc::clone(&counters);
        tokio::spawn(async move {
            let _ = connection.await;
            counters.open.fetch_sub(1, Ordering::AcqRel);
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
