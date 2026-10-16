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

// Connections, TLS and users, against the real server.

fn user_store(database: &str, user: &str, password: &str) -> Store {
    let (host, port, _, _) = server();
    let mut config = Config::new(host, port, database);
    config.user = user.to_owned();
    config.password = password.to_owned();
    Store::new(config).expect("a valid config")
}

fn tls_store(database: &str, ca: &std::path::Path, server_name: Option<&str>) -> Store {
    let setting = std::env::var("DIONAEA_TEST_CLICKHOUSE_TLS")
        .unwrap_or_else(|_| "127.0.0.1:8443".to_owned());
    let (host, port) = setting
        .rsplit_once(':')
        .map(|(h, p)| (h.to_owned(), p.parse().expect("a port")))
        .expect("host:port");
    let (_, _, user, password) = server();
    let mut config = Config::new(host, port, database);
    config.user = user;
    config.password = password;
    config.tls = Some(dionaea_store::TlsConfig {
        ca_file: Some(ca.to_path_buf()),
        server_name: Some(server_name.unwrap_or("localhost").to_owned()),
    });
    Store::new(config).expect("a valid config")
}

fn dev_ca() -> std::path::PathBuf {
    std::env::var("DIONAEA_TEST_CLICKHOUSE_CA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.dev-certs/ca.pem")
        })
}

#[test]
#[ignore = "needs ClickHouse"]
fn connections_are_reused_against_the_real_server() {
    let scratch = Scratch::new();
    for _ in 0..20 {
        scratch.store.raw_query("SELECT 1").expect("query");
    }
    scratch
        .store
        .insert_events(&[event(1)], "reuse")
        .expect("insert");
    assert_eq!(
        scratch.store.connections_opened(),
        1,
        "twenty-one calls on one connection"
    );
}

#[test]
#[ignore = "needs ClickHouse with HTTPS (compose.tls.yaml)"]
fn tls_works_against_the_real_server_on_one_connection() {
    let scratch = Scratch::new();
    let store = tls_store(&scratch.database, &dev_ca(), None);
    store.ping().expect("ping over TLS");
    store
        .insert_events(&(0..5).map(event).collect::<Vec<_>>(), "tls-1")
        .expect("insert over TLS");
    assert_eq!(count(&store, "events"), 5);
    for _ in 0..10 {
        store.raw_query("SELECT 1").expect("query");
    }
    assert_eq!(
        store.connections_opened(),
        1,
        "one handshake serves every call"
    );
    assert_eq!(
        count(&scratch.store, "events"),
        5,
        "the plain and the TLS paths reach the same data"
    );
}

#[test]
#[ignore = "needs ClickHouse with HTTPS (compose.tls.yaml)"]
fn tls_refuses_a_certificate_it_was_not_told_to_trust_and_a_wrong_name() {
    let scratch = Scratch::new();
    // A CA that did not sign the server's certificate.
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let other =
        std::env::temp_dir().join(format!("dionaea-store-other-ca-{}.pem", std::process::id()));
    std::fs::write(&other, params.self_signed(&key).expect("ca").pem()).expect("write");
    let error = tls_store(&scratch.database, &other, None)
        .ping()
        .expect_err("an untrusted server");
    assert!(matches!(error, Error::Io(_)), "{error:?}");

    // The right CA, but the name the certificate must carry is another one.
    let error = tls_store(&scratch.database, &dev_ca(), Some("other.example"))
        .ping()
        .expect_err("a wrong name");
    assert!(matches!(error, Error::Io(_)), "{error:?}");
}

fn roles(tag: &str) -> dionaea_store::Roles {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    dionaea_store::Roles {
        ingest_user: format!("ing_{tag}_{}", std::process::id()),
        ingest_password: format!("{nanos:032x}"),
        query_user: format!("qry_{tag}_{}", std::process::id()),
        query_password: format!("{:032x}", nanos ^ 0xdead_beef),
    }
}

/// Drops the users when the test ends.
struct UsersGuard<'a>(&'a Store, Vec<String>);

impl Drop for UsersGuard<'_> {
    fn drop(&mut self) {
        for user in &self.1 {
            let _ = self.0.raw_query(&format!("DROP USER IF EXISTS {user}"));
        }
    }
}

fn denied(error: &Error) -> bool {
    matches!(error, Error::Server { status: 403, .. })
        || matches!(error, Error::Server { message, .. } if message.contains("497"))
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_ingest_user_can_only_insert_and_the_query_user_can_only_read() {
    let scratch = Scratch::new();
    let roles = roles("a");
    let _cleanup = UsersGuard(
        &scratch.admin,
        vec![roles.ingest_user.clone(), roles.query_user.clone()],
    );
    scratch.store.provision_roles(&roles).expect("provision");

    let ingest = user_store(
        &scratch.database,
        &roles.ingest_user,
        &roles.ingest_password,
    );
    ingest
        .insert_events(&(0..4).map(event).collect::<Vec<_>>(), "role-1")
        .expect("the ingest user inserts");
    ingest
        .insert_drops(
            &[DropRow {
                site_id: 7,
                event_name: "e".to_owned(),
                reason: "r".to_owned(),
                at_ms: 1_760_000_000_000,
                count: 2,
            }],
            "role-d",
        )
        .expect("and counts drops");
    for sql in [
        "SELECT count() FROM events",
        "SELECT * FROM drops_daily",
        "ALTER TABLE events DELETE WHERE 1",
        "DROP TABLE events",
        "TRUNCATE TABLE events",
        "CREATE TABLE evil (x UInt8) ENGINE = Memory",
        "INSERT INTO _migrations (version, name) VALUES (99, 'x')",
        "CREATE USER eve",
    ] {
        let error = ingest.raw_query(sql).expect_err(sql);
        assert!(denied(&error), "{sql}: {error:?}");
    }

    let query = user_store(&scratch.database, &roles.query_user, &roles.query_password);
    assert_eq!(
        count(&query, "events"),
        4,
        "the query user reads what was inserted"
    );
    let error = query
        .insert_events(&[event(50)], "role-2")
        .expect_err("the query user cannot insert");
    assert!(denied(&error), "{error:?}");
    assert!(
        error.to_string().contains("ACCESS_DENIED"),
        "the redacted summary still names the error: {error}"
    );
    for sql in [
        "ALTER TABLE events DELETE WHERE 1",
        "DROP TABLE events",
        "CREATE TABLE evil (x UInt8) ENGINE = Memory",
        "INSERT INTO _migrations (version, name) VALUES (99, 'x')",
    ] {
        let error = query.raw_query(sql).expect_err(sql);
        assert!(denied(&error), "{sql}: {error:?}");
    }

    // Neither user could change anything: the data is as the ingest user left it.
    assert_eq!(count(&scratch.store, "events"), 4);
    assert_eq!(count(&scratch.store, "_migrations"), 1);
}

#[test]
#[ignore = "needs ClickHouse"]
fn provisioning_again_rotates_the_passwords() {
    let scratch = Scratch::new();
    let first = roles("b");
    let _cleanup = UsersGuard(
        &scratch.admin,
        vec![first.ingest_user.clone(), first.query_user.clone()],
    );
    scratch.store.provision_roles(&first).expect("provision");
    user_store(
        &scratch.database,
        &first.ingest_user,
        &first.ingest_password,
    )
    .ping()
    .expect("the first password works");

    let mut second = first.clone();
    second.ingest_password = format!("{:032x}", 0x1234_5678_9abc_def0_u128);
    scratch
        .store
        .provision_roles(&second)
        .expect("provision again");
    let error = user_store(
        &scratch.database,
        &first.ingest_user,
        &first.ingest_password,
    )
    .ping()
    .expect_err("the old password is dead");
    assert!(matches!(error, Error::Server { .. }), "{error:?}");
    user_store(
        &scratch.database,
        &second.ingest_user,
        &second.ingest_password,
    )
    .ping()
    .expect("the new password works");
}

#[test]
#[ignore = "needs ClickHouse"]
fn lines_encoded_earlier_insert_the_same_rows_and_dedupe_by_token() {
    let scratch = Scratch::new();
    let events: Vec<EventRow> = (0..6).map(event).collect();
    let drops = [DropRow {
        site_id: 7,
        event_name: "e".to_owned(),
        reason: "r".to_owned(),
        at_ms: 1_760_000_000_000,
        count: 3,
    }];
    // Lines built apart (as a spool record would hold them) and joined.
    let first = Store::encode_events(&events[..3]).expect("encode");
    let second = Store::encode_events(&events[3..]).expect("encode");
    let joined = format!("{first}{second}");
    scratch
        .store
        .insert_event_lines(&joined, "lines-1")
        .expect("insert");
    scratch
        .store
        .insert_event_lines(&joined, "lines-1")
        .expect("retry");
    assert_eq!(
        count(&scratch.store, "events"),
        6,
        "the retry stored nothing"
    );

    let drop_lines = Store::encode_drops(&drops).expect("encode");
    scratch
        .store
        .insert_drop_lines(&drop_lines, "lines-d")
        .expect("insert");
    scratch
        .store
        .insert_drop_lines(&drop_lines, "lines-d")
        .expect("retry");
    assert_eq!(
        scratch
            .store
            .raw_query("SELECT sum(count) FROM drops_daily")
            .expect("query")
            .trim(),
        "3"
    );
}
