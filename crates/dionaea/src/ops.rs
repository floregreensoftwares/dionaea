//! The operator endpoints, served on their own address so they need not be
//! reachable from the internet: `/healthz` (the process is up), `/readyz`
//! (this node can take events) and `/metrics` (Prometheus text).
//!
//! Metric labels come from fixed sets (outcomes, drop reasons, tables), never
//! from request content, so their number cannot grow with what senders send.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use dionaea_ingest::BatcherStats;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::collect::Collect;
use crate::net::NetCounters;

/// What the background threads tell the operator endpoints.
#[derive(Debug)]
pub(crate) struct Status {
    /// ClickHouse has been prepared (tables exist) at least once.
    pub(crate) clickhouse_ready: AtomicBool,
    /// The last attempt to read keys and schemas failed.
    pub(crate) metadata_failing: AtomicBool,
    pub(crate) batcher: Mutex<Option<Arc<BatcherStats>>>,
    pub(crate) spool_quota_bytes: u64,
}

impl Status {
    pub(crate) fn new(spool_quota_bytes: u64) -> Self {
        Self {
            clickhouse_ready: AtomicBool::new(false),
            metadata_failing: AtomicBool::new(false),
            batcher: Mutex::new(None),
            spool_quota_bytes,
        }
    }
}

pub(crate) struct Ops {
    pub(crate) collect: Arc<Collect>,
    pub(crate) status: Arc<Status>,
    pub(crate) public: Arc<NetCounters>,
}

pub(crate) fn routes(ops: Arc<Ops>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(ops)
}

/// Why this node cannot take events now, if it cannot. A ClickHouse outage is
/// not a reason: the spool absorbs it.
fn not_ready(ops: &Ops) -> Option<String> {
    let spool = ops.collect.spool.stats();
    if let Some(failure) = spool.failed {
        return Some(format!("the spool is failing: {failure}"));
    }
    let limit = ops.status.spool_quota_bytes / 100 * 95;
    if spool.bytes_used >= limit {
        return Some("the spool is nearly full".to_owned());
    }
    if ops.status.metadata_failing.load(Ordering::Relaxed) {
        return Some("the metadata store is unreachable".to_owned());
    }
    None
}

async fn readyz(State(ops): State<Arc<Ops>>) -> impl IntoResponse {
    match tokio::task::spawn_blocking(move || not_ready(&ops)).await {
        Ok(None) => (StatusCode::OK, "ready\n".to_owned()),
        Ok(Some(reason)) => (StatusCode::SERVICE_UNAVAILABLE, format!("{reason}\n")),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "unknown\n".to_owned()),
    }
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn render(ops: &Ops) -> String {
    let mut out = String::with_capacity(4096);
    let counters = &ops.collect.counters;
    let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);

    metric(&mut out, "dionaea_up", "gauge", "The process is running.");
    let _ = writeln!(out, "dionaea_up 1");

    metric(
        &mut out,
        "dionaea_requests_total",
        "counter",
        "Collect requests by outcome.",
    );
    for (outcome, value) in [
        ("accepted", &counters.accepted),
        ("bad_request", &counters.bad_request),
        ("unknown_key", &counters.unknown_key),
        ("origin_refused", &counters.origin_refused),
        ("too_large", &counters.too_large),
        ("rate_limited", &counters.rate_limited),
        ("unavailable", &counters.unavailable),
    ] {
        let _ = writeln!(
            out,
            "dionaea_requests_total{{outcome=\"{outcome}\"}} {}",
            read(value)
        );
    }

    metric(
        &mut out,
        "dionaea_events_total",
        "counter",
        "Events in accepted requests, kept or dropped.",
    );
    let _ = writeln!(
        out,
        "dionaea_events_total{{result=\"accepted\"}} {}",
        read(&counters.events_accepted)
    );
    let _ = writeln!(
        out,
        "dionaea_events_total{{result=\"dropped\"}} {}",
        read(&counters.events_dropped)
    );
    metric(
        &mut out,
        "dionaea_events_dropped_total",
        "counter",
        "Events dropped, by reason.",
    );
    for (reason, count) in counters.drops_by_reason() {
        let _ = writeln!(
            out,
            "dionaea_events_dropped_total{{reason=\"{reason}\"}} {count}"
        );
    }

    metric(
        &mut out,
        "dionaea_connections_open",
        "gauge",
        "Open connections on the public listener.",
    );
    let _ = writeln!(out, "dionaea_connections_open {}", ops.public.open());
    metric(
        &mut out,
        "dionaea_connections_total",
        "counter",
        "Connections on the public listener, accepted or refused at the limit.",
    );
    let _ = writeln!(
        out,
        "dionaea_connections_total{{result=\"accepted\"}} {}",
        read(&ops.public.accepted)
    );
    let _ = writeln!(
        out,
        "dionaea_connections_total{{result=\"refused\"}} {}",
        read(&ops.public.refused)
    );

    let spool = ops.collect.spool.stats();
    for (name, kind, help, value) in [
        (
            "dionaea_spool_bytes",
            "gauge",
            "Disk used by the spool.",
            spool.bytes_used,
        ),
        (
            "dionaea_spool_pending_bytes",
            "gauge",
            "Bytes waiting for the spool writer.",
            spool.pending_bytes,
        ),
        (
            "dionaea_spool_segments",
            "gauge",
            "Segment files in the spool.",
            spool.segments,
        ),
        (
            "dionaea_spool_quota_bytes",
            "gauge",
            "The most disk the spool may use.",
            ops.status.spool_quota_bytes,
        ),
        (
            "dionaea_spool_appended_records_total",
            "counter",
            "Records made durable since start.",
            spool.appended_records,
        ),
        (
            "dionaea_spool_syncs_total",
            "counter",
            "fsync calls made for appends since start.",
            spool.syncs,
        ),
        (
            "dionaea_spool_failed",
            "gauge",
            "1 when the spool refuses appends after a disk failure.",
            u64::from(spool.failed.is_some()),
        ),
    ] {
        metric(&mut out, name, kind, help);
        let _ = writeln!(out, "{name} {value}");
    }
    metric(
        &mut out,
        "dionaea_spool_oldest_segment_age_seconds",
        "gauge",
        "Age of the oldest segment still on disk.",
    );
    let _ = writeln!(
        out,
        "dionaea_spool_oldest_segment_age_seconds {}",
        spool.oldest_segment_age.map_or(0, |age| age.as_secs())
    );

    metric(
        &mut out,
        "dionaea_clickhouse_ready",
        "gauge",
        "1 once ClickHouse has been prepared and the batcher started.",
    );
    let _ = writeln!(
        out,
        "dionaea_clickhouse_ready {}",
        u8::from(ops.status.clickhouse_ready.load(Ordering::Relaxed))
    );
    let batcher = ops
        .status
        .batcher
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Some(stats) = batcher {
        for (name, kind, help, value) in [
            (
                "dionaea_batcher_batches_total",
                "counter",
                "Inserts ClickHouse confirmed.",
                stats.batches(),
            ),
            (
                "dionaea_batcher_retries_total",
                "counter",
                "Inserts repeated after a transient failure.",
                stats.retries(),
            ),
            (
                "dionaea_batcher_skipped_records_total",
                "counter",
                "Records given up on: unreadable or rejected for good.",
                stats.skipped_records(),
            ),
            (
                "dionaea_batcher_duplicates_total",
                "counter",
                "Events left out because another node or batch already stored them.",
                stats.duplicates(),
            ),
            (
                "dionaea_batcher_failing",
                "gauge",
                "1 while the last insert failed.",
                u64::from(stats.last_error().is_some()),
            ),
        ] {
            metric(&mut out, name, kind, help);
            let _ = writeln!(out, "{name} {value}");
        }
        metric(
            &mut out,
            "dionaea_batcher_rows_total",
            "counter",
            "Rows ClickHouse confirmed, by table.",
        );
        let _ = writeln!(
            out,
            "dionaea_batcher_rows_total{{table=\"events\"}} {}",
            stats.event_rows()
        );
        let _ = writeln!(
            out,
            "dionaea_batcher_rows_total{{table=\"drops_daily\"}} {}",
            stats.drop_rows()
        );
    }

    metric(
        &mut out,
        "dionaea_metadata_failing",
        "gauge",
        "1 while keys and schemas cannot be refreshed.",
    );
    let _ = writeln!(
        out,
        "dionaea_metadata_failing {}",
        u8::from(ops.status.metadata_failing.load(Ordering::Relaxed))
    );
    metric(
        &mut out,
        "dionaea_cache_generation",
        "gauge",
        "The generation of the keys and schemas in use.",
    );
    let _ = writeln!(
        out,
        "dionaea_cache_generation {}",
        ops.collect.cache.generation()
    );
    metric(
        &mut out,
        "dionaea_dedup_remembered_ids",
        "gauge",
        "Event ids remembered to drop retried events.",
    );
    let _ = writeln!(
        out,
        "dionaea_dedup_remembered_ids {}",
        ops.collect.remembered_ids()
    );
    out
}

async fn metrics(State(ops): State<Arc<Ops>>) -> impl IntoResponse {
    let text = tokio::task::spawn_blocking(move || render(&ops))
        .await
        .unwrap_or_default();
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        text,
    )
}
