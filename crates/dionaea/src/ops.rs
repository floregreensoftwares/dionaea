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
use dionaea_ingest::{BatcherStats, SymbolStats};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::collect::Collect;
use crate::erasure::JobStats;
use crate::net::NetCounters;
use crate::retention::RetentionStats;

/// What the background threads tell the operator endpoints.
#[derive(Debug)]
pub(crate) struct Status {
    /// ClickHouse has been prepared (tables exist) at least once.
    pub(crate) clickhouse_ready: AtomicBool,
    /// How many operational settings an operator stored, and how many of them
    /// were ignored because they are unknown or out of range.
    pub(crate) settings_stored: AtomicU64,
    pub(crate) settings_skipped: AtomicU64,
    /// The last attempt to read keys and schemas failed.
    pub(crate) metadata_failing: AtomicBool,
    pub(crate) batcher: Mutex<Option<Arc<BatcherStats>>>,
    /// What the symbolication of error stacks did, set once at start.
    symbols: std::sync::OnceLock<Arc<SymbolStats>>,
    /// What the erasure job did and the state of the requests.
    pub(crate) erasure: Arc<JobStats>,
    /// What the retention job did.
    pub(crate) retention: Arc<RetentionStats>,
    pub(crate) spool_quota_bytes: u64,
}

impl Status {
    pub(crate) fn set_symbols(&self, stats: Arc<SymbolStats>) {
        let _ = self.symbols.set(stats);
    }

    pub(crate) fn new(spool_quota_bytes: u64) -> Self {
        Self {
            clickhouse_ready: AtomicBool::new(false),
            settings_stored: AtomicU64::new(0),
            settings_skipped: AtomicU64::new(0),
            metadata_failing: AtomicBool::new(false),
            batcher: Mutex::new(None),
            symbols: std::sync::OnceLock::new(),
            erasure: Arc::default(),
            retention: Arc::default(),
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

fn render_retention(out: &mut String, job: &RetentionStats) {
    for (name, kind, help, value) in [
        (
            "dionaea_retention_runs_total",
            "counter",
            "Runs of the retention job by this server.",
            job.runs.load(Ordering::Relaxed),
        ),
        (
            "dionaea_retention_failures_total",
            "counter",
            "Runs of the retention job that failed.",
            job.failures.load(Ordering::Relaxed),
        ),
        (
            "dionaea_retention_failing",
            "gauge",
            "1 while the last run of the retention job failed.",
            u64::from(job.failing.load(Ordering::Relaxed)),
        ),
        (
            "dionaea_retention_rows_expired_total",
            "counter",
            "Rows removed because their site no longer keeps them.",
            job.rows_expired.load(Ordering::Relaxed),
        ),
        (
            "dionaea_retention_partitions_dropped_total",
            "counter",
            "Months dropped whole because no site keeps them.",
            job.partitions_dropped.load(Ordering::Relaxed),
        ),
        (
            "dionaea_retention_partitions_rewritten_total",
            "counter",
            "Months rewritten to remove expired rows from disk.",
            job.partitions_rewritten.load(Ordering::Relaxed),
        ),
        (
            "dionaea_retention_last_success_timestamp_seconds",
            "gauge",
            "When the last run that succeeded finished (0: none yet).",
            u64::try_from(job.last_success.load(Ordering::Relaxed)).unwrap_or(0),
        ),
    ] {
        metric(out, name, kind, help);
        let _ = writeln!(out, "{name} {value}");
    }
}

fn render_erasure(out: &mut String, job: &JobStats) {
    let summary = job.summary();
    metric(
        out,
        "dionaea_erasure_requests",
        "gauge",
        "Erasure requests by state, as of the last round of the job.",
    );
    for (state, count) in [
        ("requested", summary.requested),
        ("hidden", summary.hidden),
        ("complete", summary.complete),
        ("failed", summary.failed),
    ] {
        let _ = writeln!(out, "dionaea_erasure_requests{{state=\"{state}\"}} {count}");
    }
    let age = summary.oldest_open_at.map_or(0, |at| {
        let now = i64::try_from(crate::collect::system_clock() / 1000).unwrap_or(i64::MAX);
        now.saturating_sub(at).max(0)
    });
    for (name, kind, help, value) in [
        (
            "dionaea_erasure_retrying",
            "gauge",
            "Requests waiting to be tried again after trouble.",
            summary.retrying,
        ),
        (
            "dionaea_erasure_oldest_open_seconds",
            "gauge",
            "How long the oldest unfinished erasure request has waited.",
            u64::try_from(age).unwrap_or(0),
        ),
        (
            "dionaea_erasure_leader",
            "gauge",
            "1 on the server that does the erasure work.",
            u64::from(job.leader.load(Ordering::Relaxed)),
        ),
        (
            "dionaea_erasure_hidden_total",
            "counter",
            "Requests this server hid.",
            job.hidden.load(Ordering::Relaxed),
        ),
        (
            "dionaea_erasure_completed_total",
            "counter",
            "Requests this server completed.",
            job.completed.load(Ordering::Relaxed),
        ),
        (
            "dionaea_erasure_failed_total",
            "counter",
            "Requests this server failed: rejected, rows left, or given up on.",
            job.failed.load(Ordering::Relaxed),
        ),
        (
            "dionaea_erasure_trouble_total",
            "counter",
            "Steps that met trouble retrying may fix.",
            job.trouble.load(Ordering::Relaxed),
        ),
        (
            "dionaea_erasure_purged_total",
            "counter",
            "Failed requests that forgot their user id and devices.",
            job.purged.load(Ordering::Relaxed),
        ),
        (
            "dionaea_erasure_gave_up_total",
            "counter",
            "Requests given up on after too many troubles in a row.",
            job.gave_up.load(Ordering::Relaxed),
        ),
    ] {
        metric(out, name, kind, help);
        let _ = writeln!(out, "{name} {value}");
    }
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
        "Collect and identify requests by outcome.",
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
        "dionaea_identifies_total",
        "counter",
        "Identify requests: linked, or ignored because the sender is an automated client or the user was erased.",
    );
    let _ = writeln!(
        out,
        "dionaea_identifies_total{{result=\"linked\"}} {}",
        read(&counters.identified)
    );
    let _ = writeln!(
        out,
        "dionaea_identifies_total{{result=\"bot\"}} {}",
        read(&counters.identify_bots)
    );
    let _ = writeln!(
        out,
        "dionaea_identifies_total{{result=\"erased\"}} {}",
        read(&counters.identify_erased)
    );
    metric(
        &mut out,
        "dionaea_identify_traits_stripped_total",
        "counter",
        "Traits removed from identify requests: undeclared or with an invalid value.",
    );
    let _ = writeln!(
        out,
        "dionaea_identify_traits_stripped_total {}",
        read(&counters.traits_stripped)
    );

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
                "dionaea_batcher_erased_rows_total",
                "counter",
                "Rows left out because their user or device was erased after they were durable.",
                stats.erased_rows(),
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
        let _ = writeln!(
            out,
            "dionaea_batcher_rows_total{{table=\"identity\"}} {}",
            stats.identity_rows()
        );
    }

    if let Some(stats) = ops.status.symbols.get() {
        metric(
            &mut out,
            "dionaea_symbolication_events_total",
            "counter",
            "Error events by how their stack was translated.",
        );
        for (outcome, count) in stats.outcomes() {
            let _ = writeln!(
                out,
                "dionaea_symbolication_events_total{{outcome=\"{outcome}\"}} {count}"
            );
        }
        for (name, kind, help, value) in [
            (
                "dionaea_symbolication_cache_hits_total",
                "counter",
                "Symbol file lookups answered by the cache.",
                stats.hits(),
            ),
            (
                "dionaea_symbolication_file_loads_total",
                "counter",
                "Symbol files read from storage and parsed.",
                stats.loads(),
            ),
            (
                "dionaea_symbolication_index_errors_total",
                "counter",
                "Times the index of symbol files could not be read.",
                stats.index_errors(),
            ),
            (
                "dionaea_symbolication_panics_total",
                "counter",
                "Events whose symbolication panicked and was contained.",
                stats.panics(),
            ),
            (
                "dionaea_symbolication_microseconds_total",
                "counter",
                "Time spent translating error stacks, summed over the workers.",
                stats.micros(),
            ),
            (
                "dionaea_symbolication_cache_bytes",
                "gauge",
                "Memory the cache of parsed symbol files counts.",
                stats.cache_bytes() as u64,
            ),
            (
                "dionaea_symbolication_cache_entries",
                "gauge",
                "Symbol files, found or not, the cache remembers.",
                stats.cache_entries() as u64,
            ),
        ] {
            metric(&mut out, name, kind, help);
            let _ = writeln!(out, "{name} {value}");
        }
    }

    render_erasure(&mut out, &ops.status.erasure);
    render_retention(&mut out, &ops.status.retention);

    metric(
        &mut out,
        "dionaea_settings_stored",
        "gauge",
        "Operational settings stored by an operator.",
    );
    let _ = writeln!(
        out,
        "dionaea_settings_stored {}",
        ops.status.settings_stored.load(Ordering::Relaxed)
    );
    metric(
        &mut out,
        "dionaea_settings_skipped",
        "gauge",
        "Stored settings ignored because they are unknown or out of range.",
    );
    let _ = writeln!(
        out,
        "dionaea_settings_skipped {}",
        ops.status.settings_skipped.load(Ordering::Relaxed)
    );
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
