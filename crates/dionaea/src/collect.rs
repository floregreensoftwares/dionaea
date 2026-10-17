//! `POST /v1/collect` and `POST /v1/identify`: the HTTP face of the ingest path. It admits the write
//! key and origin, applies the rate limits, reads the body within its size and
//! time limits, hands it to the pipeline and answers only after the spool has
//! made the events durable. The rules live in `dionaea-ingest`; this module
//! maps them to status codes.
//!
//! Nothing here logs or stores the client address, the key or the payload: the
//! address keys a token bucket and a country lookup, and requests are counted
//! by outcome.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, CONTENT_LENGTH, CONTENT_TYPE, ORIGIN, RETRY_AFTER, USER_AGENT, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::routing::any;
use dionaea_core::MAX_BODY_LEN;
use dionaea_ingest::{
    Admission, Cache, Context, CountryLookup, Deduper, DropAggregator, Enrichment, Limits,
    Processed, RateLimiter, Refusal, RetryAfter, enqueue, enqueue_drop_rows, enqueue_identified,
    process_identify, process_with,
};
use dionaea_meta::is_write_key_shape;
use dionaea_spool::{Spool, SpoolError};
use http_body_util::{BodyExt, LengthLimitError, Limited};

use crate::settings::Tuning;

/// The header the SDKs send the write key in.
pub(crate) const KEY_HEADER: &str = "x-dionaea-key";
/// The query parameter for clients that cannot set headers (`sendBeacon`).
pub(crate) const KEY_PARAM: &str = "key";

type Clock = dyn Fn() -> u64 + Send + Sync;

/// Distinct drop counts held before they are written early.
const DROP_KEYS: usize = 20_000;

/// How the endpoint is bounded.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) limits: Limits,
    /// How long a client has to deliver the whole body.
    pub(crate) body_timeout: Duration,
    /// The `Retry-After` of a `503`, in seconds.
    pub(crate) retry_after_busy: u64,
    pub(crate) key_rate_per_sec: f64,
    pub(crate) key_burst: f64,
    pub(crate) address_rate_per_sec: f64,
    pub(crate) address_burst: f64,
    /// How many keys and addresses each limiter tracks.
    pub(crate) tracked: usize,
    /// Reverse proxies in front of the server, and the header they append the
    /// client address to. With 0 hops the socket peer is the client.
    pub(crate) trusted_proxy_hops: u8,
    pub(crate) client_address_header: String,
    pub(crate) dedup_capacity: usize,
    pub(crate) dedup_window: Duration,
    pub(crate) drop_flush: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            body_timeout: Duration::from_secs(10),
            retry_after_busy: 5,
            key_rate_per_sec: 200.0,
            key_burst: 400.0,
            address_rate_per_sec: 50.0,
            address_burst: 100.0,
            tracked: 100_000,
            trusted_proxy_hops: 0,
            client_address_header: "x-forwarded-for".to_owned(),
            dedup_capacity: 1_000_000,
            dedup_window: Duration::from_secs(600),
            drop_flush: Duration::from_secs(5),
        }
    }
}

impl Settings {
    pub(crate) fn from_tuning(tuning: &Tuning) -> Self {
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Self {
            limits: Limits {
                max_past_ms: millis(tuning.max_event_age),
                max_future_ms: millis(tuning.max_event_future),
            },
            body_timeout: tuning.body_timeout,
            retry_after_busy: tuning.retry_after_secs,
            key_rate_per_sec: tuning.key_rate_per_sec,
            key_burst: tuning.key_burst,
            address_rate_per_sec: tuning.address_rate_per_sec,
            address_burst: tuning.address_burst,
            tracked: tuning.tracked_keys,
            trusted_proxy_hops: tuning.trusted_proxy_hops,
            client_address_header: tuning.client_address_header.clone(),
            dedup_capacity: tuning.dedup_capacity,
            dedup_window: tuning.dedup_window,
            drop_flush: tuning.drop_flush,
        }
    }
}

/// Requests by outcome, for the metrics endpoint.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) accepted: AtomicU64,
    pub(crate) events_accepted: AtomicU64,
    pub(crate) events_dropped: AtomicU64,
    pub(crate) bad_request: AtomicU64,
    pub(crate) unknown_key: AtomicU64,
    pub(crate) origin_refused: AtomicU64,
    pub(crate) too_large: AtomicU64,
    pub(crate) rate_limited: AtomicU64,
    pub(crate) unavailable: AtomicU64,
    /// Identify requests that linked an anonymous id, and those ignored
    /// because the sender is an automated client.
    pub(crate) identified: AtomicU64,
    pub(crate) identify_bots: AtomicU64,
    /// Identify requests for an erased user or device, ignored.
    pub(crate) identify_erased: AtomicU64,
    /// Traits removed by the guard from identify requests.
    pub(crate) traits_stripped: AtomicU64,
    /// Events dropped, by reason. The reasons are a fixed small set, so this
    /// never grows with what senders send.
    drops: Mutex<BTreeMap<&'static str, u64>>,
}

impl Counters {
    fn add_drops(&self, processed: &Processed, invalid: u64) {
        let mut drops = self.drops.lock().unwrap_or_else(PoisonError::into_inner);
        for drop in &processed.drops {
            *drops.entry(drop.reason.as_str()).or_default() += drop.count;
        }
        if invalid > 0 {
            *drops.entry(dionaea_ingest::INVALID_ROW).or_default() += invalid;
        }
    }

    /// Events dropped so far, by reason.
    pub(crate) fn drops_by_reason(&self) -> Vec<(&'static str, u64)> {
        self.drops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(reason, count)| (*reason, *count))
            .collect()
    }
}

/// What the endpoint needs, shared by every request.
pub(crate) struct Collect {
    pub(crate) cache: Cache,
    pub(crate) spool: Spool,
    key_limiter: RateLimiter<String>,
    address_limiter: RateLimiter<IpAddr>,
    deduper: Deduper,
    drops: DropAggregator,
    geo: Option<Arc<dyn CountryLookup>>,
    settings: RwLock<Arc<Settings>>,
    clock: Box<Clock>,
    pub(crate) counters: Counters,
}

impl std::fmt::Debug for Collect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collect").finish_non_exhaustive()
    }
}

impl Collect {
    pub(crate) fn new(
        cache: Cache,
        spool: Spool,
        settings: Settings,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        let window = u64::try_from(settings.dedup_window.as_millis()).unwrap_or(u64::MAX);
        Self {
            key_limiter: RateLimiter::new(
                settings.key_rate_per_sec,
                settings.key_burst,
                settings.tracked,
            ),
            address_limiter: RateLimiter::new(
                settings.address_rate_per_sec,
                settings.address_burst,
                settings.tracked,
            ),
            deduper: Deduper::new(settings.dedup_capacity, window),
            drops: DropAggregator::new(DROP_KEYS),
            geo: None,
            cache,
            spool,
            settings: RwLock::new(Arc::new(settings)),
            clock: Box::new(clock),
            counters: Counters::default(),
        }
    }

    /// The settings in force. A request reads them once, so a change never
    /// splits one request between two sets.
    pub(crate) fn settings(&self) -> Arc<Settings> {
        Arc::clone(&self.settings.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Puts new settings into force: the limiters and the event id memory are
    /// changed in place, the rest is read by the next request.
    pub(crate) fn apply(&self, settings: Settings) {
        self.key_limiter.reconfigure(
            settings.key_rate_per_sec,
            settings.key_burst,
            settings.tracked,
        );
        self.address_limiter.reconfigure(
            settings.address_rate_per_sec,
            settings.address_burst,
            settings.tracked,
        );
        let window = u64::try_from(settings.dedup_window.as_millis()).unwrap_or(u64::MAX);
        self.deduper.reconfigure(settings.dedup_capacity, window);
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(settings);
    }

    /// Uses `lookup` to give events a country.
    #[must_use]
    pub(crate) fn with_country_lookup(mut self, lookup: Arc<dyn CountryLookup>) -> Self {
        self.geo = Some(lookup);
        self
    }

    /// Keeps the remembered event ids in a file as well, so a restart does not
    /// forget them.
    pub(crate) fn with_dedup_journal(mut self, path: &std::path::Path) -> std::io::Result<Self> {
        let settings = self.settings();
        let window = u64::try_from(settings.dedup_window.as_millis()).unwrap_or(u64::MAX);
        self.deduper = Deduper::open(settings.dedup_capacity, window, path, (self.clock)())?;
        Ok(self)
    }

    /// Makes the journal durable; the refresher calls it regularly.
    pub(crate) fn sync_dedup(&self) -> std::io::Result<()> {
        self.deduper.sync()
    }

    /// Writes the drop counts of requests that kept no event to the spool.
    /// Counts that cannot be written stay for the next call.
    pub(crate) fn flush_drops(&self) {
        let rows = self.drops.take();
        if rows.is_empty() {
            return;
        }
        if let Err((error, rest)) = enqueue_drop_rows(&self.spool, rows) {
            tracing::warn!(%error, "cannot write drop counts to the spool");
            self.drops.restore(rest);
        }
    }

    /// How often the refresher flushes drop counts.
    pub(crate) fn drop_flush_every(&self) -> Duration {
        self.settings().drop_flush
    }

    /// Whether enough distinct drop counts are held to flush them early.
    pub(crate) fn drops_full(&self) -> bool {
        self.drops.len() >= DROP_KEYS
    }

    /// How many event ids are remembered for the duplicate check.
    pub(crate) fn remembered_ids(&self) -> usize {
        self.deduper.len()
    }
}

pub(crate) fn routes(collect: Arc<Collect>) -> Router {
    Router::new()
        .route("/v1/collect", any(handle_collect))
        .route("/v1/identify", any(handle_identify))
        .with_state(collect)
}

/// Which endpoint a request is for. Admission, limits and the body are the
/// same; only what is made of the body differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Collect,
    Identify,
}

async fn handle_collect(
    state: State<Arc<Collect>>,
    peer: ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    handle(Kind::Collect, state, peer, request).await
}

async fn handle_identify(
    state: State<Arc<Collect>>,
    peer: ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    handle(Kind::Identify, state, peer, request).await
}

/// Milliseconds since the Unix epoch.
pub(crate) fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What an address is limited as: an IPv6 client by its /64, since one host
/// controls a whole prefix and could otherwise rotate through it; an IPv4
/// address mapped into IPv6 as the IPv4 address.
pub(crate) fn address_bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || {
                let octets = v6.octets();
                let mut prefix = [0u8; 16];
                prefix[..8].copy_from_slice(&octets[..8]);
                IpAddr::V6(Ipv6Addr::from(prefix))
            },
            IpAddr::V4,
        ),
        v4 => v4,
    }
}

/// The client's address. With no trusted proxy it is the socket peer. With
/// `hops` proxies in front, each appends the address it saw to the header, so
/// the client is the `hops`-th entry from the right; anything to its left was
/// written by the client and is ignored. A header that is missing, too short
/// or not an address falls back to the peer.
pub(crate) fn client_address(headers: &HeaderMap, peer: IpAddr, hops: u8, header: &str) -> IpAddr {
    if hops == 0 {
        return peer;
    }
    let entries: Vec<&str> = headers
        .get_all(header)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect();
    let hops = usize::from(hops);
    entries
        .len()
        .checked_sub(hops)
        .and_then(|index| entries.get(index))
        .and_then(|entry| entry.parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

fn json(status: StatusCode, body: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(body.to_owned()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn error(status: StatusCode, reason: &str) -> Response<Body> {
    json(status, &format!("{{\"error\":\"{reason}\"}}"))
}

fn with_retry_after(mut response: Response<Body>, seconds: u64) -> Response<Body> {
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

/// Adds the CORS answer for an origin that was admitted.
fn with_cors(mut response: Response<Body>, origin: Option<&HeaderValue>) -> Response<Body> {
    if let Some(origin) = origin {
        let headers = response.headers_mut();
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(VARY, HeaderValue::from_static("Origin"));
    }
    response
}

/// The write key of a request and where it was found. `Err` when the header
/// and the parameter both exist and differ.
fn key_of(headers: &HeaderMap, uri: &Uri) -> Result<Option<String>, ()> {
    let from_header = headers
        .get(KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let from_query = uri.query().and_then(|query| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == KEY_PARAM)
            .map(|(_, value)| value)
    });
    match (from_header, from_query) {
        (Some(a), Some(b)) if a != b => Err(()),
        (Some(a), _) => Ok(Some(a.to_owned())),
        (None, Some(b)) => Ok(Some(b.to_owned())),
        (None, None) => Ok(None),
    }
}

fn is_json_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            let media = value.split(';').next().unwrap_or("").trim();
            media.eq_ignore_ascii_case("application/json")
                || media.eq_ignore_ascii_case("text/plain")
        })
}

/// How reading the body ended.
enum BodyRead {
    Complete(axum::body::Bytes),
    TooLarge,
    Unreadable,
    TimedOut,
}

async fn read_body(body: Body, timeout: Duration) -> BodyRead {
    let limited = Limited::new(body, MAX_BODY_LEN);
    match tokio::time::timeout(timeout, limited.collect()).await {
        Ok(Ok(collected)) => BodyRead::Complete(collected.to_bytes()),
        Ok(Err(error)) if error.downcast_ref::<LengthLimitError>().is_some() => BodyRead::TooLarge,
        Ok(Err(_)) => BodyRead::Unreadable,
        Err(_) => BodyRead::TimedOut,
    }
}

async fn handle(
    kind: Kind,
    State(collect): State<Arc<Collect>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let counters = &collect.counters;
    let settings = collect.settings();
    let (parts, body) = request.into_parts();
    let preflight = parts.method == Method::OPTIONS;
    if !preflight && parts.method != Method::POST {
        let mut response = error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
        response
            .headers_mut()
            .insert("allow", HeaderValue::from_static("POST, OPTIONS"));
        return response;
    }

    // The key: found, well formed and known, before anything is read.
    let key = match key_of(&parts.headers, &parts.uri) {
        Ok(Some(key)) if is_write_key_shape(&key) => key,
        Ok(_) => {
            counters.unknown_key.fetch_add(1, Ordering::Relaxed);
            return error(StatusCode::UNAUTHORIZED, "unknown_key");
        }
        Err(()) => {
            counters.bad_request.fetch_add(1, Ordering::Relaxed);
            return error(StatusCode::BAD_REQUEST, "conflicting_keys");
        }
    };
    let now = (collect.clock)();
    let client = client_address(
        &parts.headers,
        peer.ip(),
        settings.trusted_proxy_hops,
        &settings.client_address_header,
    );
    if let Err(RetryAfter(seconds)) = collect.address_limiter.check(address_bucket(client), now) {
        counters.rate_limited.fetch_add(1, Ordering::Relaxed);
        return with_retry_after(
            error(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            seconds,
        );
    }
    let origin = parts.headers.get(ORIGIN);
    let admission = match collect
        .cache
        .admit(&key, origin.and_then(|v| v.to_str().ok()))
    {
        Ok(admission) => admission,
        Err(Refusal::UnknownKey) => {
            counters.unknown_key.fetch_add(1, Ordering::Relaxed);
            return error(StatusCode::UNAUTHORIZED, "unknown_key");
        }
        Err(Refusal::OriginNotAllowed) => {
            counters.origin_refused.fetch_add(1, Ordering::Relaxed);
            return error(StatusCode::FORBIDDEN, "origin_not_allowed");
        }
    };

    if preflight {
        let mut response = with_cors(Response::new(Body::empty()), origin);
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("POST"),
        );
        headers.insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("content-type, x-dionaea-key"),
        );
        headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        return response;
    }

    if let Err(RetryAfter(seconds)) = collect.key_limiter.check(key, now) {
        counters.rate_limited.fetch_add(1, Ordering::Relaxed);
        return with_cors(
            with_retry_after(
                error(StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
                seconds,
            ),
            origin,
        );
    }
    if !is_json_type(&parts.headers) {
        counters.bad_request.fetch_add(1, Ordering::Relaxed);
        return with_cors(
            error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type"),
            origin,
        );
    }

    // Size first: the declared length, then the bytes actually read.
    let declared = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|length| length > MAX_BODY_LEN) {
        counters.too_large.fetch_add(1, Ordering::Relaxed);
        return with_cors(
            error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
            origin,
        );
    }
    let bytes = match read_body(body, settings.body_timeout).await {
        BodyRead::Complete(bytes) => bytes,
        BodyRead::TooLarge => {
            counters.too_large.fetch_add(1, Ordering::Relaxed);
            return with_cors(
                error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
                origin,
            );
        }
        BodyRead::Unreadable => {
            counters.bad_request.fetch_add(1, Ordering::Relaxed);
            return with_cors(error(StatusCode::BAD_REQUEST, "body_unreadable"), origin);
        }
        BodyRead::TimedOut => {
            counters.bad_request.fetch_add(1, Ordering::Relaxed);
            return with_cors(error(StatusCode::REQUEST_TIMEOUT, "body_timeout"), origin);
        }
    };

    // What the request says about its sender. The address is used for the
    // country and goes no further.
    let user_agent = parts.headers.get(USER_AGENT).and_then(|v| v.to_str().ok());
    let enrichment = Enrichment::of_request(user_agent, Some(client), collect.geo.as_deref());

    // Decoding, the guard and the durable write block the thread (CPU and an
    // fsync wait), so they run off the async workers.
    let work = Arc::clone(&collect);
    let outcome = tokio::task::spawn_blocking(move || match kind {
        Kind::Collect => store(&work, &admission, &bytes, now, &enrichment),
        Kind::Identify => store_identify(&work, &admission, &bytes, now, &enrichment),
    })
    .await;
    let response = match outcome {
        Ok(Outcome::Stored { accepted, dropped }) => {
            counters.accepted.fetch_add(1, Ordering::Relaxed);
            counters
                .events_accepted
                .fetch_add(accepted as u64, Ordering::Relaxed);
            counters
                .events_dropped
                .fetch_add(dropped, Ordering::Relaxed);
            json(
                StatusCode::ACCEPTED,
                &format!("{{\"accepted\":{accepted},\"dropped\":{dropped}}}"),
            )
        }
        Ok(Outcome::Linked { stripped }) => {
            counters.accepted.fetch_add(1, Ordering::Relaxed);
            counters.identified.fetch_add(1, Ordering::Relaxed);
            counters
                .traits_stripped
                .fetch_add(stripped, Ordering::Relaxed);
            json(
                StatusCode::ACCEPTED,
                &format!("{{\"accepted\":1,\"stripped\":{stripped}}}"),
            )
        }
        Ok(Outcome::Ignored { erased }) => {
            counters.accepted.fetch_add(1, Ordering::Relaxed);
            let seen = if erased {
                &counters.identify_erased
            } else {
                &counters.identify_bots
            };
            seen.fetch_add(1, Ordering::Relaxed);
            json(StatusCode::ACCEPTED, "{\"accepted\":0,\"stripped\":0}")
        }
        Ok(Outcome::Refused(reason)) => {
            counters.bad_request.fetch_add(1, Ordering::Relaxed);
            error(StatusCode::BAD_REQUEST, reason)
        }
        Ok(Outcome::Unavailable) | Err(_) => {
            counters.unavailable.fetch_add(1, Ordering::Relaxed);
            with_retry_after(
                error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
                settings.retry_after_busy,
            )
        }
    };
    with_cors(response, origin)
}

enum Outcome {
    Stored {
        accepted: usize,
        dropped: u64,
    },
    /// An identify request that was stored, and how many traits the guard
    /// removed.
    Linked {
        stripped: u64,
    },
    /// An identify request from an automated client or for an erased user or
    /// device: acknowledged, counted, not stored.
    Ignored {
        erased: bool,
    },
    Refused(&'static str),
    Unavailable,
}

/// What a spool that would not take a record means for the sender.
fn spool_failure(error: &SpoolError) -> Outcome {
    match error {
        SpoolError::Full
        | SpoolError::Busy
        | SpoolError::Closed
        | SpoolError::Io(_)
        | SpoolError::Locked
        | SpoolError::BadCommit => Outcome::Unavailable,
        // The request cannot be a valid one: too large for a record, or empty.
        SpoolError::TooLarge | SpoolError::Empty => Outcome::Refused("body_too_large"),
    }
}

fn store_identify(
    collect: &Collect,
    admission: &Admission,
    body: &[u8],
    now: u64,
    enrichment: &Enrichment,
) -> Outcome {
    let identified = match process_identify(admission, body, enrichment) {
        Ok(identified) => identified,
        Err(error) => return Outcome::Refused(error.as_str()),
    };
    if identified.bot || identified.erased {
        // Nothing to store: only the count, merged with the other drop counts.
        collect.drops.add(admission.site_id, &identified.drops, now);
        return Outcome::Ignored {
            erased: identified.erased,
        };
    }
    match enqueue_identified(&collect.spool, &identified, now) {
        Ok(()) => Outcome::Linked {
            stripped: identified.stripped,
        },
        Err(error) => spool_failure(&error),
    }
}

fn store(
    collect: &Collect,
    admission: &Admission,
    body: &[u8],
    now: u64,
    enrichment: &Enrichment,
) -> Outcome {
    let settings = collect.settings();
    let context = Context {
        enrichment: Some(enrichment),
        deduper: Some(&collect.deduper),
    };
    let processed: Processed = match process_with(admission, body, now, &settings.limits, &context)
    {
        Ok(processed) => processed,
        Err(error) => return Outcome::Refused(error.as_str()),
    };
    let dropped: u64 = processed.drops.iter().map(|d| d.count).sum();
    if processed.accepted.is_empty() {
        // Nothing to make durable: only counts, merged and written every few
        // seconds instead of one spool record per request.
        collect.drops.add(admission.site_id, &processed.drops, now);
        collect.counters.add_drops(&processed, 0);
        return Outcome::Stored {
            accepted: 0,
            dropped,
        };
    }
    match enqueue(&collect.spool, &processed, admission.site_id, now) {
        Ok(stored) => {
            // Only now are the events durable, so only now may a retry of
            // them be taken for a duplicate.
            collect
                .deduper
                .remember(processed.accepted.iter().map(|a| a.event_key), now);
            collect
                .counters
                .add_drops(&processed, stored.invalid as u64);
            Outcome::Stored {
                accepted: stored.events,
                // Events that passed the guard but could not become a row are
                // dropped too.
                dropped: dropped + stored.invalid as u64,
            }
        }
        Err(error) => spool_failure(&error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use axum::body::to_bytes;
    use dionaea_meta::{MetaStore, SqliteMeta};
    use dionaea_spool::Config as SpoolConfig;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    const SCHEMA: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n\n[traits.plan]\nkind = \"id\"\n\n[traits.seats]\nkind = \"count\"\nmax = 1000\n";
    const NOW: u64 = 1_760_000_000_000;
    const PEER: ([u8; 4], u16) = ([203, 0, 113, 9], 5555);
    const OTHER_KEY: &str = "wk_aaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct World {
        router: Router,
        collect: Arc<Collect>,
        meta: SqliteMeta,
        key: String,
        dir: std::path::PathBuf,
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn world(origins: &[&str], settings: Settings, spool_quota: Option<u64>) -> World {
        static N: AtomicUsize = AtomicUsize::new(0);
        let meta = SqliteMeta::open_in_memory().unwrap().with_hash_key(
            dionaea_meta::HashKey::from_secret("0123456789abcdef0123456789abcdef").expect("key"),
        );
        let organization = meta.create_organization("o").unwrap();
        let site = meta.create_site(organization.id, "s").unwrap();
        meta.publish_schema(site.id, SCHEMA).unwrap();
        let origins: Vec<String> = origins.iter().map(|o| (*o).to_owned()).collect();
        let key = meta.issue_write_key(site.id, "k", &origins).unwrap().key;
        let cache = Cache::new();
        cache.refresh(&meta).unwrap();

        let dir = std::env::temp_dir().join(format!(
            "dionaea-collect-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = SpoolConfig::new(&dir);
        config.sync = false;
        if let Some(quota) = spool_quota {
            config.quota_bytes = quota;
            config.segment_bytes = quota / 2;
        }
        let spool = Spool::open(config).unwrap();
        let collect = Arc::new(Collect::new(cache, spool, settings, || NOW));
        World {
            router: routes(Arc::clone(&collect)),
            collect,
            meta,
            key,
            dir,
        }
    }

    fn event(n: u32, name: &str, props: Value) -> Value {
        json!({
            "event_id": format!("01890000-0000-7000-8000-{n:012}"),
            "ts": NOW - 1000,
            "name": name,
            "anon_id": "anon1",
            "props": props,
        })
    }

    fn body(events: &[Value]) -> Vec<u8> {
        serde_json::to_vec(&json!({"sent_at": NOW, "sdk": "t/1", "events": events})).unwrap()
    }

    fn peer() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(PEER))
    }

    fn post(key: Option<&str>, body: Vec<u8>) -> Request<Body> {
        let mut builder = Request::post("/v1/collect")
            .header(CONTENT_TYPE, "application/json")
            .extension(peer());
        if let Some(key) = key {
            builder = builder.header(KEY_HEADER, key);
        }
        builder.body(Body::from(body)).unwrap()
    }

    async fn send(world: &World, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
        let response = world.router.clone().oneshot(request).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, value)
    }

    fn spooled(world: &World) -> usize {
        let records = world.collect.spool.read(1000, usize::MAX).unwrap();
        world.collect.spool.rewind();
        records.len()
    }

    #[tokio::test]
    async fn a_valid_batch_is_acknowledged_after_it_is_in_the_spool() {
        let w = world(&[], Settings::default(), None);
        let events = [
            event(1, "checkout", json!({"items": 2})),
            event(2, "checkout", json!({})),
            event(3, "mystery", json!({})),
        ];
        let (status, _, answer) = send(&w, post(Some(&w.key), body(&events))).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer, json!({"accepted": 1, "dropped": 2}));
        assert_eq!(spooled(&w), 1);
        let c = &w.collect.counters;
        assert_eq!(c.accepted.load(Ordering::Relaxed), 1);
        assert_eq!(c.events_accepted.load(Ordering::Relaxed), 1);
        assert_eq!(c.events_dropped.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn an_empty_batch_is_valid_and_stores_nothing() {
        let w = world(&[], Settings::default(), None);
        let (status, _, answer) = send(&w, post(Some(&w.key), body(&[]))).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer, json!({"accepted": 0, "dropped": 0}));
        assert_eq!(spooled(&w), 0);
    }

    #[tokio::test]
    async fn the_key_may_come_in_the_query_for_clients_that_cannot_set_headers() {
        let w = world(&[], Settings::default(), None);
        let request = Request::post(format!("/v1/collect?x=1&key={}", w.key))
            .header(CONTENT_TYPE, "text/plain;charset=UTF-8")
            .extension(peer())
            .body(Body::from(body(&[event(
                1,
                "checkout",
                json!({"items": 1}),
            )])))
            .unwrap();
        let (status, _, answer) = send(&w, request).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer["accepted"], 1);
    }

    #[tokio::test]
    async fn two_different_keys_in_one_request_are_refused_and_the_same_key_twice_is_fine() {
        let w = world(&[], Settings::default(), None);
        let request = Request::post(format!("/v1/collect?key={OTHER_KEY}"))
            .header(CONTENT_TYPE, "application/json")
            .header(KEY_HEADER, &w.key)
            .extension(peer())
            .body(Body::from(body(&[])))
            .unwrap();
        let (status, _, answer) = send(&w, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(answer["error"], "conflicting_keys");
        let request = Request::post(format!("/v1/collect?key={}", w.key))
            .header(CONTENT_TYPE, "application/json")
            .header(KEY_HEADER, &w.key)
            .extension(peer())
            .body(Body::from(body(&[])))
            .unwrap();
        assert_eq!(send(&w, request).await.0, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_missing_malformed_unknown_or_revoked_key_is_401() {
        let w = world(&[], Settings::default(), None);
        for key in [None, Some(""), Some("nope"), Some(OTHER_KEY)] {
            let (status, _, answer) = send(&w, post(key, body(&[]))).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{key:?}");
            assert_eq!(answer["error"], "unknown_key");
        }
        w.meta.revoke_write_key(&w.key).unwrap();
        let cached = send(&w, post(Some(&w.key), body(&[]))).await.0;
        assert_eq!(
            cached,
            StatusCode::ACCEPTED,
            "the cache is stale until refreshed"
        );
        w.collect.cache.refresh(&w.meta).unwrap();
        let revoked = send(&w, post(Some(&w.key), body(&[]))).await.0;
        assert_eq!(revoked, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn only_post_and_options_are_answered() {
        let w = world(&[], Settings::default(), None);
        let request = Request::get("/v1/collect")
            .extension(peer())
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(&w, request).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(headers["allow"], "POST, OPTIONS");
    }

    #[tokio::test]
    async fn a_key_with_origins_needs_one_of_them_and_the_answer_echoes_it() {
        let w = world(&["https://shop.example"], Settings::default(), None);
        let with_origin = |origin: Option<&str>| {
            let batch = body(&[event(1, "checkout", json!({"items": 1}))]);
            let mut request = post(Some(&w.key), batch);
            if let Some(origin) = origin {
                request
                    .headers_mut()
                    .insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            request
        };
        let (status, headers, _) = send(&w, with_origin(Some("https://shop.example"))).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(headers[ACCESS_CONTROL_ALLOW_ORIGIN], "https://shop.example");
        assert_eq!(headers[VARY], "Origin");
        for bad in [Some("https://evil.example"), None] {
            let (status, headers, answer) = send(&w, with_origin(bad)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{bad:?}");
            assert_eq!(answer["error"], "origin_not_allowed");
            assert!(
                headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
                "no CORS answer for a refused origin"
            );
        }
    }

    #[tokio::test]
    async fn the_preflight_is_answered_only_for_a_valid_key_and_origin() {
        let w = world(&["https://shop.example"], Settings::default(), None);
        let preflight = |key: &str, origin: &str| {
            Request::options(format!("/v1/collect?key={key}"))
                .header(ORIGIN, origin)
                .extension(peer())
                .body(Body::empty())
                .unwrap()
        };
        let (status, headers, _) = send(&w, preflight(&w.key, "https://shop.example")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(headers[ACCESS_CONTROL_ALLOW_ORIGIN], "https://shop.example");
        assert_eq!(headers[ACCESS_CONTROL_ALLOW_METHODS], "POST");
        let allowed = headers[ACCESS_CONTROL_ALLOW_HEADERS].to_str().unwrap();
        assert!(allowed.contains("x-dionaea-key"), "{allowed}");
        let evil = send(&w, preflight(&w.key, "https://evil.example")).await.0;
        assert_eq!(evil, StatusCode::FORBIDDEN);
        let unknown = send(&w, preflight(OTHER_KEY, "https://shop.example"))
            .await
            .0;
        assert_eq!(unknown, StatusCode::UNAUTHORIZED);
        assert_eq!(spooled(&w), 0);
    }

    #[tokio::test]
    async fn the_body_type_is_checked() {
        let w = world(&[], Settings::default(), None);
        let xml = Request::post("/v1/collect")
            .header(CONTENT_TYPE, "application/xml")
            .header(KEY_HEADER, &w.key)
            .extension(peer())
            .body(Body::from(body(&[])))
            .unwrap();
        assert_eq!(send(&w, xml).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let untyped = Request::post("/v1/collect")
            .header(KEY_HEADER, &w.key)
            .extension(peer())
            .body(Body::from(body(&[])))
            .unwrap();
        assert_eq!(
            send(&w, untyped).await.0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_413_by_its_declared_length_and_by_what_arrives() {
        let w = world(&[], Settings::default(), None);
        let big = vec![b' '; MAX_BODY_LEN + 1];
        let mut declared = post(Some(&w.key), big.clone());
        declared.headers_mut().insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&big.len().to_string()).unwrap(),
        );
        let (status, _, answer) = send(&w, declared).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(answer["error"], "body_too_large");
        // No Content-Length (chunked): the limit applies while reading.
        let undeclared = send(&w, post(Some(&w.key), big)).await.0;
        assert_eq!(undeclared, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(w.collect.counters.too_large.load(Ordering::Relaxed), 2);
        // The limit itself is read, then refused as not JSON.
        let exact = send(&w, post(Some(&w.key), vec![b' '; MAX_BODY_LEN]))
            .await
            .0;
        assert_eq!(exact, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_refused_body_is_400_with_the_reason() {
        let w = world(&[], Settings::default(), None);
        let too_many: Vec<Value> = (0..101)
            .map(|n| event(n, "checkout", json!({"items": 1})))
            .collect();
        let too_many =
            serde_json::to_vec(&json!({"sent_at": NOW, "sdk": "t/1", "events": too_many})).unwrap();
        for (payload, reason) in [
            (b"not json".to_vec(), "not_json"),
            (b"[]".to_vec(), "not_an_object"),
            (b"{}".to_vec(), "invalid_field"),
            (too_many, "too_many_events"),
        ] {
            let (status, _, answer) = send(&w, post(Some(&w.key), payload)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{reason}");
            assert_eq!(answer["error"], reason);
        }
        assert_eq!(spooled(&w), 0);
    }

    #[tokio::test]
    async fn a_burst_over_the_key_limit_is_429_with_retry_after() {
        let settings = Settings {
            key_burst: 3.0,
            key_rate_per_sec: 1.0,
            ..Settings::default()
        };
        let w = world(&[], settings, None);
        for _ in 0..3 {
            let ok = send(&w, post(Some(&w.key), body(&[]))).await.0;
            assert_eq!(ok, StatusCode::ACCEPTED);
        }
        let (status, headers, answer) = send(&w, post(Some(&w.key), body(&[]))).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(answer["error"], "rate_limited");
        assert_eq!(headers[RETRY_AFTER], "1");
        assert_eq!(w.collect.counters.rate_limited.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_address_limit_applies_before_the_key_is_even_looked_up() {
        let settings = Settings {
            address_burst: 2.0,
            address_rate_per_sec: 1.0,
            ..Settings::default()
        };
        let w = world(&[], settings, None);
        for _ in 0..2 {
            let refused = send(&w, post(Some(OTHER_KEY), body(&[]))).await.0;
            assert_eq!(refused, StatusCode::UNAUTHORIZED);
        }
        let (status, headers, _) = send(&w, post(Some(OTHER_KEY), body(&[]))).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(headers.contains_key(RETRY_AFTER));
        // Another address is unaffected.
        let mut other = post(Some(OTHER_KEY), body(&[]));
        other
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([198, 51, 100, 1], 1))));
        assert_eq!(send(&w, other).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_full_spool_answers_503_with_retry_after() {
        let w = world(&[], Settings::default(), Some(4_000));
        let mut refused = None;
        for n in 0..200 {
            // A different event each time: a repeated id would be dropped as a
            // duplicate and never fill the spool.
            let batch = body(&[event(n, "checkout", json!({"items": 1}))]);
            let (status, headers, answer) = send(&w, post(Some(&w.key), batch)).await;
            if status == StatusCode::SERVICE_UNAVAILABLE {
                refused = Some((headers, answer));
                break;
            }
            assert_eq!(status, StatusCode::ACCEPTED);
        }
        let (headers, answer) = refused.expect("the quota is reached");
        assert_eq!(headers[RETRY_AFTER], "5");
        assert_eq!(answer["error"], "unavailable");
        assert!(w.collect.counters.unavailable.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn ipv6_clients_are_limited_by_their_prefix_and_mapped_ipv4_as_ipv4() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(address_bucket(a), address_bucket(b));
        assert_ne!(address_bucket(a), address_bucket(other));
        let mapped: IpAddr = "::ffff:203.0.113.9".parse().unwrap();
        assert_eq!(
            address_bucket(mapped),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        let v4: IpAddr = "198.51.100.1".parse().unwrap();
        assert_eq!(address_bucket(v4), v4);
    }

    #[tokio::test]
    async fn a_body_that_never_finishes_is_408() {
        let settings = Settings {
            body_timeout: Duration::from_millis(50),
            ..Settings::default()
        };
        let w = world(&[], settings, None);
        let stalled = Request::post("/v1/collect")
            .header(CONTENT_TYPE, "application/json")
            .header(KEY_HEADER, &w.key)
            .extension(peer())
            .body(Body::from_stream(futures_util::stream::pending::<
                Result<axum::body::Bytes, std::io::Error>,
            >()))
            .unwrap();
        let (status, _, answer) = send(&w, stalled).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(answer["error"], "body_timeout");
        assert_eq!(spooled(&w), 0);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn the_client_address_is_the_socket_peer_unless_proxies_are_trusted() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        let forwarded = headers(&[("x-forwarded-for", "1.1.1.1, 9.9.9.9")]);
        assert_eq!(client_address(&forwarded, peer, 0, "x-forwarded-for"), peer);
        let nine: IpAddr = "9.9.9.9".parse().unwrap();
        assert_eq!(client_address(&forwarded, peer, 1, "x-forwarded-for"), nine);
        let one: IpAddr = "1.1.1.1".parse().unwrap();
        assert_eq!(client_address(&forwarded, peer, 2, "x-forwarded-for"), one);
    }

    #[test]
    fn a_forwarded_header_that_cannot_be_trusted_falls_back_to_the_peer() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        for (pairs, hops) in [
            (vec![], 1),
            (vec![("x-forwarded-for", "9.9.9.9")], 2),
            (vec![("x-forwarded-for", "not-an-address")], 1),
            (vec![("x-forwarded-for", "")], 1),
            (vec![("x-other", "9.9.9.9")], 1),
        ] {
            let map = headers(&pairs);
            assert_eq!(
                client_address(&map, peer, hops, "x-forwarded-for"),
                peer,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn several_header_lines_count_as_one_list_in_order() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        let map = headers(&[
            ("x-forwarded-for", "1.1.1.1"),
            ("x-forwarded-for", "2.2.2.2, 3.3.3.3"),
        ]);
        let third: IpAddr = "3.3.3.3".parse().unwrap();
        assert_eq!(client_address(&map, peer, 1, "x-forwarded-for"), third);
        let ipv6 = headers(&[("x-forwarded-for", "2001:db8::7")]);
        assert_eq!(
            client_address(&ipv6, peer, 1, "x-forwarded-for"),
            "2001:db8::7".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn a_body_that_fails_while_arriving_is_not_called_too_large() {
        let failing = Body::from_stream(futures_util::stream::iter(vec![
            Ok(axum::body::Bytes::from_static(b"{")),
            Err(std::io::Error::other("connection reset")),
        ]));
        assert!(matches!(
            read_body(failing, Duration::from_secs(5)).await,
            BodyRead::Unreadable
        ));
        let big = Body::from(vec![b' '; MAX_BODY_LEN + 1]);
        assert!(matches!(
            read_body(big, Duration::from_secs(5)).await,
            BodyRead::TooLarge
        ));
    }

    #[tokio::test]
    async fn responses_never_carry_the_key_or_the_address() {
        let w = world(&[], Settings::default(), None);
        let batch = body(&[event(1, "checkout", json!({"items": 1}))]);
        let (_, headers, answer) = send(&w, post(Some(&w.key), batch)).await;
        let text = format!("{headers:?}{answer}");
        assert!(
            !text.contains(&w.key) && !text.contains("203.0.113"),
            "{text}"
        );
    }

    fn identify_body(traits: Option<Value>) -> Vec<u8> {
        let mut request = json!({
            "sent_at": NOW, "sdk": "t/1", "anon_id": "anon1", "user_id": "u_42"
        });
        if let Some(traits) = traits {
            request["traits"] = traits;
        }
        serde_json::to_vec(&request).unwrap()
    }

    fn identify_post(key: Option<&str>, body: Vec<u8>) -> Request<Body> {
        let mut builder = Request::post("/v1/identify")
            .header(CONTENT_TYPE, "application/json")
            .extension(peer());
        if let Some(key) = key {
            builder = builder.header(KEY_HEADER, key);
        }
        builder.body(Body::from(body)).unwrap()
    }

    /// Everything in the spool as text. Reading moves the spool's cursor, so
    /// it is put back.
    fn spool_text(world: &World) -> String {
        let records = world.collect.spool.read(1000, usize::MAX).unwrap();
        world.collect.spool.rewind();
        records
            .iter()
            .map(|record| String::from_utf8_lossy(&record.payload).into_owned())
            .collect()
    }

    #[tokio::test]
    async fn an_identify_request_is_acknowledged_after_its_link_and_traits_are_in_the_spool() {
        let w = world(&[], Settings::default(), None);
        let traits = json!({"plan": "pro", "seats": 5, "email": "a@b.c"});
        let (status, _, answer) =
            send(&w, identify_post(Some(&w.key), identify_body(Some(traits)))).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer, json!({"accepted": 1, "stripped": 1}));
        assert_eq!(spooled(&w), 1);
        let text = spool_text(&w);
        for expected in [
            "\"anon_id\":\"anon1\"",
            "\"user_id\":\"u_42\"",
            "\"plan\":\"pro\"",
            "\"seats\":5",
        ] {
            assert!(text.contains(expected), "{expected} in {text}");
        }
        assert!(
            !text.contains("a@b.c"),
            "an undeclared trait never reaches the spool"
        );
        let c = &w.collect.counters;
        assert_eq!(c.accepted.load(Ordering::Relaxed), 1);
        assert_eq!(c.identified.load(Ordering::Relaxed), 1);
        assert_eq!(c.traits_stripped.load(Ordering::Relaxed), 1);
        assert_eq!(c.events_accepted.load(Ordering::Relaxed), 0, "not an event");
    }

    #[tokio::test]
    async fn an_identify_request_without_traits_still_links() {
        let w = world(&[], Settings::default(), None);
        let (status, _, answer) = send(&w, identify_post(Some(&w.key), identify_body(None))).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer, json!({"accepted": 1, "stripped": 0}));
        let text = spool_text(&w);
        assert!(text.contains("\"user_id\":\"u_42\"") && !text.contains("traits_str"));
    }

    #[tokio::test]
    async fn identify_needs_the_same_key_origin_and_method_as_collect() {
        let w = world(&["https://shop.example"], Settings::default(), None);
        let (status, ..) = send(&w, identify_post(None, identify_body(None))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, ..) = send(&w, identify_post(Some(OTHER_KEY), identify_body(None))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, ..) = send(&w, identify_post(Some(&w.key), identify_body(None))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "no allowed origin");
        assert_eq!(spooled(&w), 0);

        let mut request = identify_post(Some(&w.key), identify_body(None));
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://shop.example"));
        let (status, headers, _) = send(&w, request).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(
            headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://shop.example"
        );

        let preflight = Request::options("/v1/identify")
            .header(KEY_HEADER, &w.key)
            .header(ORIGIN, "https://shop.example")
            .extension(peer())
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(&w, preflight).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(headers.get(ACCESS_CONTROL_ALLOW_METHODS).is_some());

        let get = Request::get("/v1/identify")
            .extension(peer())
            .body(Body::empty())
            .unwrap();
        let (status, ..) = send(&w, get).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_malformed_identify_request_is_refused_and_nothing_is_stored() {
        let w = world(&[], Settings::default(), None);
        let email = serde_json::to_vec(&json!({
            "sent_at": NOW, "sdk": "t/1", "anon_id": "a", "user_id": "a@b.c"
        }))
        .unwrap();
        let (status, _, answer) = send(&w, identify_post(Some(&w.key), email)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(answer, json!({"error": "invalid_field"}));
        let (status, ..) = send(&w, identify_post(Some(&w.key), b"nope".to_vec())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A batch of events is not an identify request.
        let (status, ..) = send(&w, identify_post(Some(&w.key), body(&[]))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(spooled(&w), 0);
        assert_eq!(w.collect.counters.identified.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn identify_shares_the_rate_limit_of_its_key() {
        let settings = Settings {
            key_burst: 2.0,
            key_rate_per_sec: 0.001,
            ..Settings::default()
        };
        let w = world(&[], settings, None);
        let (first, ..) = send(&w, identify_post(Some(&w.key), identify_body(None))).await;
        let (second, ..) = send(&w, post(Some(&w.key), body(&[]))).await;
        let (third, headers, _) = send(&w, identify_post(Some(&w.key), identify_body(None))).await;
        assert_eq!(
            (first, second, third),
            (
                StatusCode::ACCEPTED,
                StatusCode::ACCEPTED,
                StatusCode::TOO_MANY_REQUESTS
            )
        );
        assert!(headers.get(RETRY_AFTER).is_some());
    }

    #[tokio::test]
    async fn a_bot_identify_is_acknowledged_counted_and_not_stored() {
        let w = world(&[], Settings::default(), None);
        let mut request = identify_post(Some(&w.key), identify_body(Some(json!({"plan": "pro"}))));
        request
            .headers_mut()
            .insert(USER_AGENT, HeaderValue::from_static("Googlebot/2.1"));
        let (status, _, answer) = send(&w, request).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer, json!({"accepted": 0, "stripped": 0}));
        assert_eq!(spooled(&w), 0);
        assert_eq!(w.collect.counters.identify_bots.load(Ordering::Relaxed), 1);
        assert_eq!(w.collect.counters.identified.load(Ordering::Relaxed), 0);
        w.collect.flush_drops();
        let text = spool_text(&w);
        assert!(
            text.contains("(identify)") && text.contains("\"reason\":\"bot\""),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_full_spool_answers_503_to_identify_too() {
        let w = world(&[], Settings::default(), Some(4_000));
        let mut refused = None;
        for _ in 0..200 {
            let (status, headers, answer) =
                send(&w, identify_post(Some(&w.key), identify_body(None))).await;
            if status == StatusCode::SERVICE_UNAVAILABLE {
                refused = Some((headers, answer));
                break;
            }
            assert_eq!(status, StatusCode::ACCEPTED);
        }
        let (headers, answer) = refused.expect("the quota is reached");
        assert_eq!(headers[RETRY_AFTER], "5");
        assert_eq!(answer["error"], "unavailable");
        assert!(w.collect.counters.unavailable.load(Ordering::Relaxed) >= 1);
    }

    /// Asks for the erasure of `user` (with the device `anon`) on the world's
    /// site and lets the cache see it, as a node does after a refresh.
    fn erase(world: &World, user: &str, anon: &str) {
        let site = world.meta.sites().unwrap()[0].id;
        let request = world.meta.request_erasure(site, user, "test").unwrap();
        world
            .meta
            .add_erasure_anons(request.id, &[anon.to_owned()])
            .unwrap();
        assert!(world.collect.cache.refresh(&world.meta).unwrap());
    }

    #[tokio::test]
    async fn events_of_an_erased_device_are_acknowledged_counted_and_not_stored() {
        let w = world(&[], Settings::default(), None);
        erase(&w, "u_gone", "anon1");
        let events = [
            event(1, "checkout", json!({"items": 1})),
            event(2, "checkout", json!({"items": 2})),
        ];
        let (status, _, answer) = send(&w, post(Some(&w.key), body(&events))).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer, json!({"accepted": 0, "dropped": 2}));
        assert_eq!(spooled(&w), 0);
        assert_eq!(w.collect.counters.drops_by_reason(), vec![("erased", 2)]);
    }

    #[tokio::test]
    async fn an_identify_for_an_erased_user_or_device_is_ignored() {
        let w = world(&[], Settings::default(), None);
        erase(&w, "u_gone", "anon_gone");
        for (anon, user) in [("anon_gone", "u_fine"), ("anon_new", "u_gone")] {
            let body = serde_json::to_vec(&json!({
                "sent_at": NOW, "sdk": "t/1", "anon_id": anon, "user_id": user,
                "traits": {"plan": "pro"}
            }))
            .unwrap();
            let (status, _, answer) = send(&w, identify_post(Some(&w.key), body)).await;
            assert_eq!(status, StatusCode::ACCEPTED);
            assert_eq!(answer, json!({"accepted": 0, "stripped": 0}));
        }
        assert_eq!(spooled(&w), 0);
        let c = &w.collect.counters;
        assert_eq!(c.identify_erased.load(Ordering::Relaxed), 2);
        assert_eq!(c.identified.load(Ordering::Relaxed), 0);
        assert_eq!(c.identify_bots.load(Ordering::Relaxed), 0);

        // Someone else is not affected.
        let (status, _, answer) = send(&w, identify_post(Some(&w.key), identify_body(None))).await;
        assert_eq!(
            (status, answer["accepted"].as_u64()),
            (StatusCode::ACCEPTED, Some(1))
        );
    }
}
