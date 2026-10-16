//! Tests against a real ClickHouse (the one in `compose.yaml`). They are
//! ignored by default; CI and `cargo test -p dionaea-store -- --ignored` run
//! them. The server comes from `DIONAEA_TEST_CLICKHOUSE` as
//! `host:port:user:password`, defaulting to the compose file's.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use dionaea_store::{Config, DropRow, Error, EventRow, Store};
use serde_json::Value;

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

/// A store on a database of its own, dropped when the test ends.
struct Scratch {
    store: Store,
    admin: Store,
    database: String,
}

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let (host, port, user, password) = server();
        let database = format!(
            "dionaea_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let make = |database: &str| {
            let mut config = Config::new(&host, port, database);
            config.user.clone_from(&user);
            config.password.clone_from(&password);
            Store::new(config).expect("a valid config")
        };
        let scratch = Self {
            store: make(&database),
            admin: make("default"),
            database,
        };
        scratch.store.migrate().expect("migrate");
        scratch
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self
            .admin
            .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.database));
    }
}

fn event(n: u64) -> EventRow {
    EventRow {
        site_id: 7,
        event_id: format!("0190f7e2-7c3e-7a52-9a0e-{n:012x}"),
        ts_ms: 1_760_000_000_000 + n,
        received_ms: 1_760_000_001_000,
        name: "checkout".to_owned(),
        schema_version: 3,
        anon_id: format!("a{n}"),
        session_id: "s1".to_owned(),
        screen: "/orders/:id".to_owned(),
        props_str: vec![(
            "coupon".to_owned(),
            "SAVE \"10\"\n\u{1f600} caf\u{e9}".to_owned(),
        )],
        props_num: vec![("items".to_owned(), 3.0), ("ratio".to_owned(), 0.25)],
        country: "BR".to_owned(),
        platform: "ios".to_owned(),
        os: "iOS 18".to_owned(),
        browser: String::new(),
        device_class: "phone".to_owned(),
        app_version: "1.4.2+57".to_owned(),
        sdk: "dionaea-rn/0.1.0".to_owned(),
    }
}

fn count(store: &Store, table: &str) -> u64 {
    store
        .raw_query(&format!("SELECT count() FROM {table}"))
        .expect("count")
        .trim()
        .parse()
        .expect("a number")
}

#[test]
#[ignore = "needs ClickHouse"]
fn migrate_creates_the_tables_and_is_idempotent() {
    let scratch = Scratch::new();
    assert_eq!(scratch.store.migrate().expect("again"), Vec::<u32>::new());
    let tables = scratch
        .admin
        .raw_query(&format!(
            "SELECT name FROM system.tables WHERE database = '{}' ORDER BY name FORMAT TabSeparated",
            scratch.database
        ))
        .expect("tables");
    assert_eq!(
        tables.lines().collect::<Vec<_>>(),
        vec!["_migrations", "drops_daily", "events"]
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn two_nodes_migrating_at_once_both_succeed() {
    let scratch = Scratch::new();
    // The scratch database is migrated already; use a fresh one for the race.
    let (host, port, user, password) = server();
    let database = format!("{}_race", scratch.database);
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (host, user, password, database) = (
                host.clone(),
                user.clone(),
                password.clone(),
                database.clone(),
            );
            thread::spawn(move || {
                let mut config = Config::new(host, port, database);
                config.user = user;
                config.password = password;
                Store::new(config).expect("config").migrate()
            })
        })
        .collect();
    for handle in handles {
        handle
            .join()
            .expect("a thread")
            .expect("every migrator succeeds");
    }
    assert_eq!(
        scratch
            .admin
            .raw_query(&format!(
                "SELECT count(DISTINCT name) FROM system.tables WHERE database = '{database}'"
            ))
            .expect("count")
            .trim(),
        "3"
    );
    let _ = scratch
        .admin
        .raw_query(&format!("DROP DATABASE IF EXISTS {database}"));
}

#[test]
#[ignore = "needs ClickHouse"]
fn events_round_trip_with_every_column() {
    let scratch = Scratch::new();
    let rows: Vec<EventRow> = (0..3).map(event).collect();
    scratch
        .store
        .insert_events(&rows, "batch-1")
        .expect("insert");
    let out = scratch
        .store
        .raw_query("SELECT * FROM events ORDER BY ts FORMAT JSONEachRow")
        .expect("select");
    let stored: Vec<Value> = out
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    assert_eq!(stored.len(), 3);
    let first = &stored[0];
    assert_eq!(first["site_id"], 7);
    assert_eq!(first["event_id"], "0190f7e2-7c3e-7a52-9a0e-000000000000");
    assert_eq!(first["ts"], "2025-10-09 08:53:20.000");
    assert_eq!(first["received_at"], "2025-10-09 08:53:21.000");
    assert_eq!(first["name"], "checkout");
    assert_eq!(first["schema_version"], 3);
    assert_eq!(first["anon_id"], "a0");
    assert_eq!(first["screen"], "/orders/:id");
    assert_eq!(
        first["props_str"]["coupon"],
        "SAVE \"10\"\n\u{1f600} caf\u{e9}"
    );
    assert_eq!(first["props_num"]["items"], 3.0);
    assert_eq!(first["props_num"]["ratio"], 0.25);
    assert_eq!(first["country"], "BR");
    assert_eq!(first["browser"], "");
    assert_eq!(first["sdk"], "dionaea-rn/0.1.0");
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_last_millisecond_clickhouse_holds_is_stored() {
    let scratch = Scratch::new();
    let mut row = event(0);
    row.ts_ms = 10_413_791_999_999;
    scratch.store.insert_events(&[row], "edge").expect("insert");
    let stored = scratch
        .store
        .raw_query("SELECT toString(ts) FROM events")
        .expect("select");
    assert_eq!(stored.trim(), "2299-12-31 23:59:59.999");
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_retried_insert_with_the_same_token_stores_the_batch_once() {
    let scratch = Scratch::new();
    let rows: Vec<EventRow> = (0..50).map(event).collect();
    scratch
        .store
        .insert_events(&rows, "seg1-off100-n50")
        .expect("first");
    assert_eq!(count(&scratch.store, "events"), 50);

    // The retry, once directly and once after other inserts went in between.
    scratch
        .store
        .insert_events(&rows, "seg1-off100-n50")
        .expect("retry");
    assert_eq!(
        count(&scratch.store, "events"),
        50,
        "the retry stored the batch again"
    );
    let other: Vec<EventRow> = (100..110).map(event).collect();
    scratch
        .store
        .insert_events(&other, "seg1-off200-n10")
        .expect("other");
    scratch
        .store
        .insert_events(&rows, "seg1-off100-n50")
        .expect("late retry");
    assert_eq!(count(&scratch.store, "events"), 60);

    // A different token is a different batch.
    scratch
        .store
        .insert_events(&rows, "seg2-off100-n50")
        .expect("new token");
    assert_eq!(count(&scratch.store, "events"), 110);
}

#[test]
#[ignore = "needs ClickHouse"]
fn drops_are_summed_by_key_and_deduplicated_by_token() {
    let scratch = Scratch::new();
    let drop = |reason: &str, count: u64| DropRow {
        site_id: 7,
        event_name: "checkout".to_owned(),
        reason: reason.to_owned(),
        at_ms: 1_760_000_000_000,
        count,
    };
    scratch
        .store
        .insert_drops(
            &[
                drop("unknown_event", 3),
                drop("unknown_event", 4),
                drop("invalid_screen", 1),
            ],
            "d1",
        )
        .expect("insert");
    scratch
        .store
        .insert_drops(
            &[
                drop("unknown_event", 3),
                drop("unknown_event", 4),
                drop("invalid_screen", 1),
            ],
            "d1",
        )
        .expect("retry");
    let sums = scratch
        .store
        .raw_query("SELECT reason, sum(count) FROM drops_daily GROUP BY reason ORDER BY reason FORMAT TabSeparated")
        .expect("select");
    assert_eq!(
        sums.lines().collect::<Vec<_>>(),
        vec!["invalid_screen\t1", "unknown_event\t7"]
    );
    scratch
        .store
        .raw_query("OPTIMIZE TABLE drops_daily FINAL")
        .expect("optimize");
    assert_eq!(
        count(&scratch.store, "drops_daily"),
        2,
        "same keys merge into one row"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn server_errors_come_back_typed_and_without_data() {
    let scratch = Scratch::new();
    let error = scratch
        .store
        .raw_query("SELECT * FROM no_such_table")
        .expect_err("an error");
    let Error::Server { status, message } = error else {
        panic!("a server error")
    };
    assert_ne!(status, 200);
    assert!(
        message.contains("no_such_table") || message.contains("UNKNOWN_TABLE"),
        "{message}"
    );

    let (host, port, user, _) = server();
    let mut config = Config::new(host, port, "default");
    config.user = user;
    config.password = "wrong-password".to_owned();
    let error = Store::new(config)
        .expect("config")
        .ping()
        .expect_err("a refused login");
    assert!(matches!(error, Error::Server { .. }), "{error:?}");
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_batch_of_twenty_thousand_events_is_stored_in_one_insert() {
    let scratch = Scratch::new();
    let rows: Vec<EventRow> = (0..20_000).map(event).collect();
    let started = std::time::Instant::now();
    scratch.store.insert_events(&rows, "big").expect("insert");
    eprintln!("20000 events inserted in {:?}", started.elapsed());
    assert_eq!(count(&scratch.store, "events"), 20_000);
}
