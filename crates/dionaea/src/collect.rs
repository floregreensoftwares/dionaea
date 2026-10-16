//! `POST /v1/collect`: the HTTP face of the ingest path. It admits the write
//! key and origin, applies the rate limits, reads the body within its size and
//! time limits, hands it to the pipeline and answers only after the spool has
//! made the events durable. The rules live in `dionaea-ingest`; this module
//! maps them to status codes.
//!
//! Nothing here logs or stores the client address, the key or the payload: the
//! address only keys a token bucket, and requests are counted by outcome.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, CONTENT_LENGTH, CONTENT_TYPE, ORIGIN, RETRY_AFTER, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::routing::any;
use dionaea_core::MAX_BODY_LEN;
use dionaea_ingest::{
    Admission, Cache, Limits, Processed, RateLimiter, Refusal, RetryAfter, enqueue, process,
};
use dionaea_meta::is_write_key_shape;
use dionaea_spool::{Spool, SpoolError};

/// The header the SDKs send the write key in.
pub(crate) const KEY_HEADER: &str = "x-dionaea-key";
/// The query parameter for clients that cannot set headers (`sendBeacon`).
pub(crate) const KEY_PARAM: &str = "key";

type Clock = dyn Fn() -> u64 + Send + Sync;

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
}

/// What the endpoint needs, shared by every request.
pub(crate) struct Collect {
    pub(crate) cache: Cache,
    pub(crate) spool: Spool,
    key_limiter: RateLimiter<String>,
    address_limiter: RateLimiter<IpAddr>,
    settings: Settings,
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
            cache,
            spool,
            settings,
            clock: Box::new(clock),
            counters: Counters::default(),
        }
    }
}

pub(crate) fn routes(collect: Arc<Collect>) -> Router {
    Router::new()
        .route("/v1/collect", any(handle))
        .with_state(collect)
}

/// What an address is limited as: an IPv6 client by its /64, since one host
/// controls a whole prefix and could otherwise rotate through it; an IPv4
/// address mapped into IPv6 as the IPv4 address.
fn address_bucket(ip: IpAddr) -> IpAddr {
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

/// Milliseconds since the Unix epoch.
pub(crate) fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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

async fn handle(
    State(collect): State<Arc<Collect>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let counters = &collect.counters;
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
    if let Err(RetryAfter(seconds)) = collect
        .address_limiter
        .check(address_bucket(peer.ip()), now)
    {
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
    let bytes =
        match tokio::time::timeout(collect.settings.body_timeout, to_bytes(body, MAX_BODY_LEN))
            .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => {
                counters.too_large.fetch_add(1, Ordering::Relaxed);
                return with_cors(
                    error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
                    origin,
                );
            }
            Err(_) => {
                counters.bad_request.fetch_add(1, Ordering::Relaxed);
                return with_cors(error(StatusCode::REQUEST_TIMEOUT, "body_timeout"), origin);
            }
        };

    // Decoding, the guard and the durable write block the thread (CPU and an
    // fsync wait), so they run off the async workers.
    let work = Arc::clone(&collect);
    let outcome = tokio::task::spawn_blocking(move || store(&work, &admission, &bytes, now)).await;
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
        Ok(Outcome::Refused(reason)) => {
            counters.bad_request.fetch_add(1, Ordering::Relaxed);
            error(StatusCode::BAD_REQUEST, reason)
        }
        Ok(Outcome::Unavailable) | Err(_) => {
            counters.unavailable.fetch_add(1, Ordering::Relaxed);
            with_retry_after(
                error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
                collect.settings.retry_after_busy,
            )
        }
    };
    with_cors(response, origin)
}

enum Outcome {
    Stored { accepted: usize, dropped: u64 },
    Refused(&'static str),
    Unavailable,
}

fn store(collect: &Collect, admission: &Admission, body: &[u8], now: u64) -> Outcome {
    let processed: Processed = match process(admission, body, now, &collect.settings.limits) {
        Ok(processed) => processed,
        Err(error) => return Outcome::Refused(error.as_str()),
    };
    let dropped: u64 = processed.drops.iter().map(|d| d.count).sum();
    match enqueue(&collect.spool, &processed, admission.site_id, now) {
        Ok(stored) => Outcome::Stored {
            accepted: stored.events,
            // Events that passed the guard but could not become a row are
            // dropped too.
            dropped: dropped + stored.invalid as u64,
        },
        Err(SpoolError::Full | SpoolError::Busy | SpoolError::Closed | SpoolError::Io(_)) => {
            Outcome::Unavailable
        }
        // The request cannot be a valid one: too large for a record, or empty.
        Err(SpoolError::TooLarge | SpoolError::Empty) => Outcome::Refused("body_too_large"),
        Err(SpoolError::Locked | SpoolError::BadCommit) => Outcome::Unavailable,
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

    const SCHEMA: &str =
        "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n";
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
        let meta = SqliteMeta::open_in_memory().unwrap();
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
        world.collect.spool.read(1000, usize::MAX).unwrap().len()
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
        let batch = body(&[event(1, "checkout", json!({"items": 1}))]);
        let mut refused = None;
        for _ in 0..200 {
            let (status, headers, answer) = send(&w, post(Some(&w.key), batch.clone())).await;
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
}
