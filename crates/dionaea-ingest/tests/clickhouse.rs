//! The whole path against a real ClickHouse (the one in `compose.yaml`):
//! request body, guard, spool, batcher, rows. Ignored by default; CI and
//! `cargo test -p dionaea-ingest -- --ignored` run them. The server comes from
//! `DIONAEA_TEST_CLICKHOUSE` as `host:port:user:password`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use dionaea_ingest::{Batcher, BatcherConfig, Cache, Destination, Limits, enqueue, process};
use dionaea_meta::{MetaStore, SqliteMeta};
use dionaea_spool::{Config as SpoolConfig, Spool};
use dionaea_store::{Config, Store};
use serde_json::json;

const NOW: u64 = 1_760_000_000_000;
const SCHEMA: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n\n[events.checkout.fields.coupon]\nkind = \"text\"\nmax = 32\n";

fn server() -> (String, u16, String, String) {
    let setting = std::env::var("DIONAEA_TEST_CLICKHOUSE")
        .unwrap_or_else(|_| "127.0.0.1:8123:dionaea:dionaea-dev".to_owned());
    let parts: Vec<&str> = setting.splitn(4, ':').collect();
    assert_eq!(
        parts.len(),
        4,
        "DIONAEA_TEST_CLICKHOUSE is host:port:user:password"
    );
    (
        parts[0].to_owned(),
        parts[1].parse().expect("a port"),
        parts[2].to_owned(),
        parts[3].to_owned(),
    )
}

fn make(database: &str, port: Option<u16>) -> Store {
    let (host, server_port, user, password) = server();
    let mut config = Config::new(host, port.unwrap_or(server_port), database);
    config.user = user;
    config.password = password;
    config.timeout = Duration::from_secs(5);
    Store::new(config).expect("a valid config")
}

/// A database of its own, dropped when the test ends.
struct Scratch {
    database: String,
    admin: Store,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let database = format!(
            "dionaea_ingest_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = make("default", None);
        admin
            .raw_query(&format!("CREATE DATABASE {database}"))
            .expect("create the database");
        make(&database, None).migrate().expect("migrate");
        Self { database, admin }
    }

    fn store(&self) -> Store {
        make(&self.database, None)
    }

    fn number(&self, sql: &str) -> u64 {
        self.store()
            .raw_query(sql)
            .expect("query")
            .trim()
            .parse()
            .unwrap_or(0)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self
            .admin
            .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.database));
    }
}

fn spool(tag: &str) -> (Spool, PathBuf) {
    let dir = std::env::temp_dir().join(format!("dionaea-ingest-ch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = SpoolConfig::new(&dir);
    config.sync = false;
    (Spool::open(config).expect("spool"), dir)
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn fast() -> BatcherConfig {
    BatcherConfig {
        min_interval: Duration::ZERO,
        poll: Duration::from_millis(10),
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        ..BatcherConfig::default()
    }
}

fn event(n: u32, name: &str, props: serde_json::Value) -> serde_json::Value {
    json!({
        "event_id": format!("01890000-0000-7000-8000-{n:012}"),
        "ts": NOW - 1000,
        "name": name,
        "anon_id": format!("anon{n}"),
        "session_id": "s1",
        "screen": "/cart",
        "props": props,
    })
}

fn body(events: &[serde_json::Value]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "sent_at": NOW,
        "sdk": "test/1.0",
        "release": "1.4.2",
        "events": events,
    }))
    .expect("json")
}

/// A site with a schema and a write key, and a cache that knows them.
fn site() -> (Cache, String) {
    let meta = SqliteMeta::open_in_memory().expect("meta");
    let organization = meta.create_organization("o").expect("organization");
    let site = meta.create_site(organization.id, "s").expect("site");
    meta.publish_schema(site.id, SCHEMA).expect("schema");
    let key = meta.issue_write_key(site.id, "k", &[]).expect("key").key;
    let cache = Cache::new();
    cache.refresh(&meta).expect("refresh");
    (cache, key)
}

/// Runs one request through admission, the pipeline and the spool.
fn collect(cache: &Cache, key: &str, spool: &Spool, events: &[serde_json::Value]) {
    let admission = cache.admit(key, None).expect("admitted");
    let processed =
        process(&admission, &body(events), NOW, &Limits::default()).expect("a valid batch");
    enqueue(spool, &processed, admission.site_id, NOW).expect("durable");
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_request_becomes_rows_and_drop_counts() {
    let scratch = Scratch::new();
    let (cache, key) = site();
    let (spool, dir) = spool("flow");
    collect(
        &cache,
        &key,
        &spool,
        &[
            event(
                1,
                "checkout",
                json!({"items": 2, "coupon": "SPRING", "secret": "stripped"}),
            ),
            event(2, "checkout", json!({"items": 5})),
            event(3, "checkout", json!({"items": 1})),
            event(4, "checkout", json!({})),
            event(5, "mystery", json!({"items": 1})),
        ],
    );

    let store: Arc<dyn Destination> = Arc::new(scratch.store());
    let batcher = Batcher::start(spool.clone(), store, fast()).expect("thread");
    wait_for("the rows", || {
        scratch.number("SELECT count() FROM events") == 3
    });
    wait_for("the drop counts", || {
        scratch.number("SELECT sum(count) FROM drops_daily") == 2
    });
    batcher.stop();

    assert_eq!(
        scratch.number("SELECT sum(props_num['items']) FROM events WHERE site_id = 1"),
        8
    );
    assert_eq!(
        scratch.number("SELECT count() FROM events WHERE props_str['coupon'] = 'SPRING'"),
        1
    );
    assert_eq!(
        scratch.number("SELECT count() FROM events WHERE mapContains(props_str, 'secret')"),
        0,
        "undeclared properties never reach the table"
    );
    assert_eq!(
        scratch.number(
            "SELECT sum(count) FROM drops_daily WHERE event_name = 'checkout' AND reason = 'missing_required_field'"
        ),
        1
    );
    assert_eq!(
        scratch.number(
            "SELECT sum(count) FROM drops_daily WHERE event_name = '(undeclared)' AND reason = 'unknown_event'"
        ),
        1,
        "an invented name is counted under the placeholder"
    );
    drop(spool);
    let reopened = Spool::open(SpoolConfig::new(&dir)).expect("reopen");
    assert!(
        reopened.read(10, 1 << 20).expect("read").is_empty(),
        "the spool drained"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn an_outage_loses_nothing_and_the_spool_drains_when_clickhouse_returns() {
    let scratch = Scratch::new();
    let (cache, key) = site();
    let (spool, _dir) = spool("outage");

    // Nothing listens on port 1: ClickHouse is "down".
    let down: Arc<dyn Destination> = Arc::new(make(&scratch.database, Some(1)));
    let batcher = Batcher::start(spool.clone(), down, fast()).expect("thread");
    for n in 0..20 {
        collect(
            &cache,
            &key,
            &spool,
            &[
                event(n * 2, "checkout", json!({"items": 1})),
                event(n * 2 + 1, "checkout", json!({"items": 2})),
            ],
        );
    }
    wait_for("failed attempts", || batcher.stats().retries() >= 3);
    assert_eq!(scratch.number("SELECT count() FROM events"), 0);
    assert!(batcher.stats().last_error().is_some());
    batcher.stop();

    // ClickHouse is back: a new batcher drains the same spool.
    let up: Arc<dyn Destination> = Arc::new(scratch.store());
    let batcher = Batcher::start(spool.clone(), up, fast()).expect("thread");
    wait_for("every row", || {
        scratch.number("SELECT count() FROM events") == 40
    });
    wait_for("the batcher to confirm", || {
        batcher.stats().event_rows() == 40
    });
    batcher.stop();
    assert_eq!(
        scratch.number("SELECT uniqExact(event_id) FROM events"),
        40,
        "nothing was stored twice"
    );
}
