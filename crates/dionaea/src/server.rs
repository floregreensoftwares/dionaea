use std::future::Future;

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::clickhouse;
use crate::config::{Config, clickhouse_endpoint};

pub(crate) fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Serves until `shutdown` resolves, then stops accepting and drains
/// in-flight requests.
pub(crate) async fn run(
    config: &Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(config.listen).await?;
    info!(listen = %listener.local_addr()?, "listening");

    if let Ok((host, port)) = clickhouse_endpoint(&config.clickhouse_url) {
        let url = config.clickhouse_url.clone();
        tokio::spawn(async move {
            match clickhouse::ping(&host, port).await {
                Ok(()) => info!(clickhouse = %url, "clickhouse reachable"),
                Err(reason) => warn!(clickhouse = %url, %reason, "clickhouse not reachable"),
            }
        });
    }

    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown)
        .await?;
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
        let response = router()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_path_is_not_found() {
        let response = router()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
