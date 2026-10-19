//! Tests against a real ClickHouse (the one in `compose.yaml`). They are
//! ignored by default; CI and `cargo test -p dionaea-store -- --ignored` run
//! them. The server comes from `DIONAEA_TEST_CLICKHOUSE` as
//! `host:port:user:password`, defaulting to the compose file's.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use dionaea_store::{AliasRow, Config, DropRow, Error, EventRow, Expired, Store, TraitsRow};
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

    /// Another handle to the same database, as a second node would have.
    fn fresh(&self) -> Store {
        let (host, port, user, password) = server();
        let mut config = Config::new(host, port, &self.database);
        config.user = user;
        config.password = password;
        Store::new(config).expect("a valid config")
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
        vec![
            "_migrations",
            "derived_sessions",
            "drops_daily",
            "errors",
            "errors_mv",
            "event_ids",
            "events",
            "identity_alias",
            "issues_daily",
            "issues_daily_mv",
            "measurements",
            "measurements_mv",
            "perf_daily",
            "perf_daily_mv",
            "releases",
            "releases_mv",
            "sessions",
            "sessions_mv",
            "user_traits"
        ]
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
        "19"
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
    assert_eq!(count(&scratch.store, "_migrations"), 8);
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

#[test]
#[ignore = "needs ClickHouse"]
fn recorded_ids_are_found_by_any_node_and_recording_twice_stores_them_once() {
    let scratch = Scratch::new();
    let a = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";
    let b = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6f";
    let c = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d70";
    let lines = Store::encode_event_ids([(7, a), (8, b)], now_ms()).expect("encode");
    scratch
        .store
        .insert_event_id_lines(&lines, "ids-1")
        .expect("insert");
    scratch
        .store
        .insert_event_id_lines(&lines, "ids-1")
        .expect("retry");
    assert_eq!(
        count(&scratch.store, "event_ids"),
        2,
        "the retry stored nothing"
    );

    // Another handle (another node) sees them; the site is part of the key.
    let other = scratch.fresh();
    let known = other
        .known_event_ids(&[(7, a), (7, b), (8, b), (8, c), (9, a)])
        .expect("lookup");
    let mut found: Vec<_> = known.into_iter().collect();
    found.sort();
    assert_eq!(found, [(7, a.to_owned()), (8, b.to_owned())]);
    assert!(other.known_event_ids(&[]).expect("empty").is_empty());

    // A lookup of the largest size works.
    let many: Vec<(u32, String)> = (0..dionaea_store::MAX_IDS_PER_LOOKUP)
        .map(|n| (u32::MAX, format!("0190f7e2-7c3e-7a52-9a0e-{n:012x}")))
        .collect();
    let keys: Vec<(u32, &str)> = many.iter().map(|(s, i)| (*s, i.as_str())).collect();
    assert!(
        other
            .known_event_ids(&keys)
            .expect("large lookup")
            .is_empty()
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_ingest_user_can_read_the_event_ids_and_nothing_else() {
    let scratch = Scratch::new();
    let roles = roles("ids");
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
    let id = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";
    let lines = Store::encode_event_ids([(7, id)], now_ms()).expect("encode");
    ingest
        .insert_event_id_lines(&lines, "role-ids")
        .expect("the ingest user records ids");
    let known = ingest
        .known_event_ids(&[(7, id)])
        .expect("and looks them up");
    assert!(known.contains(&(7, id.to_owned())));
    for sql in [
        "SELECT count() FROM events",
        "ALTER TABLE event_ids DELETE WHERE 1",
        "DROP TABLE event_ids",
    ] {
        let error = ingest.raw_query(sql).expect_err(sql);
        assert!(denied(&error), "{sql}: {error:?}");
    }
}

/// The current time in milliseconds. Recorded ids expire after a day (a TTL
/// that ClickHouse also applies on insert), so these tests record "now".
fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis(),
    )
    .expect("fits")
}

fn alias(anon: &str, user: &str, at: u64) -> AliasRow {
    AliasRow {
        site_id: 7,
        anon_id: anon.to_owned(),
        user_id: user.to_owned(),
        linked_ms: at,
    }
}

#[test]
#[ignore = "needs ClickHouse"]
fn links_keep_every_pair_and_the_latest_wins_for_an_anonymous_id() {
    let scratch = Scratch::new();
    let t = now_ms();
    let rows = [
        alias("a1", "u1", t),
        alias("a1", "u2", t + 5_000),
        alias("a2", "u1", t + 1_000),
        // The same pair again, later: one row, with the later time.
        alias("a1", "u1", t + 9_000),
    ];
    let lines = Store::encode_aliases(&rows).expect("encode");
    scratch
        .store
        .insert_alias_lines(&lines, "alias-1")
        .expect("insert");
    // The same token repeats nothing.
    scratch
        .store
        .insert_alias_lines(&lines, "alias-1")
        .expect("insert again");
    let pairs = scratch
        .store
        .raw_query(
            "SELECT anon_id, user_id FROM identity_alias FINAL ORDER BY anon_id, user_id FORMAT TabSeparated",
        )
        .expect("pairs");
    assert_eq!(
        pairs.lines().collect::<Vec<_>>(),
        vec!["a1\tu1", "a1\tu2", "a2\tu1"],
        "both users of a1 are kept"
    );
    let latest = scratch
        .store
        .raw_query(
            "SELECT anon_id, argMax(user_id, linked_at) FROM identity_alias FINAL GROUP BY anon_id ORDER BY anon_id FORMAT TabSeparated",
        )
        .expect("latest");
    assert_eq!(
        latest.lines().collect::<Vec<_>>(),
        vec!["a1\tu1", "a2\tu1"],
        "the latest link of a1 is the one made last"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_latest_traits_of_a_user_replace_the_earlier_ones() {
    let scratch = Scratch::new();
    let t = now_ms();
    let traits = |plan: &str, seats: f64, at: u64| TraitsRow {
        site_id: 7,
        user_id: "u1".to_owned(),
        traits_str: vec![("plan".to_owned(), plan.to_owned())],
        traits_num: vec![("seats".to_owned(), seats)],
        updated_ms: at,
    };
    let first = Store::encode_traits(&[traits("free \"x\"\n\u{1f600}", 1.0, t)]).expect("encode");
    let second = Store::encode_traits(&[traits("pro", 5.0, t + 1_000)]).expect("encode");
    // Delivered out of order: the later time still wins.
    scratch
        .store
        .insert_traits_lines(&second, "t-2")
        .expect("insert");
    scratch
        .store
        .insert_traits_lines(&first, "t-1")
        .expect("insert");
    let row = scratch
        .store
        .raw_query(
            "SELECT traits_str['plan'], traits_num['seats'] FROM user_traits FINAL WHERE site_id = 7 AND user_id = 'u1' FORMAT TabSeparated",
        )
        .expect("traits");
    assert_eq!(row.trim(), "pro\t5");
    assert_eq!(count(&scratch.store, "user_traits FINAL"), 1);
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_ingest_user_can_write_identity_and_read_none_of_it() {
    let scratch = Scratch::new();
    let roles = roles("identity");
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
    let t = now_ms();
    ingest
        .insert_alias_lines(
            &Store::encode_aliases(&[alias("a1", "u1", t)]).expect("encode"),
            "role-alias",
        )
        .expect("the ingest user links");
    ingest
        .insert_traits_lines(
            &Store::encode_traits(&[TraitsRow {
                site_id: 7,
                user_id: "u1".to_owned(),
                traits_str: vec![],
                traits_num: vec![],
                updated_ms: t,
            }])
            .expect("encode"),
            "role-traits",
        )
        .expect("and stores traits");
    for sql in [
        "SELECT count() FROM identity_alias",
        "SELECT count() FROM user_traits",
        "ALTER TABLE identity_alias DELETE WHERE 1",
    ] {
        let error = ingest.raw_query(sql).expect_err(sql);
        assert!(denied(&error), "{sql}: {error:?}");
    }
    assert_eq!(count(&scratch.store, "identity_alias"), 1);
    assert_eq!(count(&scratch.store, "user_traits"), 1);
}

fn rows(counts: &[dionaea_store::TableCount]) -> Vec<(&'static str, u64)> {
    counts.iter().map(|c| (c.table, c.rows)).collect()
}

/// Two users on one site. u1 has devices a1 and a2, u2 has a3; a2 was also
/// used once by u2. Another site has a user with the same name.
fn seed_two_users(scratch: &Scratch) {
    let t = now_ms();
    let mut events = Vec::new();
    for (n, anon, site) in [
        (1, "a1", 7),
        (2, "a1", 7),
        (3, "a2", 7),
        (4, "a3", 7),
        (5, "a1", 8),
    ] {
        let mut row = event(n);
        row.anon_id = anon.to_owned();
        row.site_id = site;
        events.push(row);
    }
    scratch
        .store
        .insert_events(&events, "erase-events")
        .expect("events");
    let link = |site, anon: &str, user: &str| AliasRow {
        site_id: site,
        anon_id: anon.to_owned(),
        user_id: user.to_owned(),
        linked_ms: t,
    };
    scratch
        .store
        .insert_alias_lines(
            &Store::encode_aliases(&[
                link(7, "a1", "u1"),
                link(7, "a2", "u1"),
                link(7, "a2", "u2"),
                link(7, "a3", "u2"),
                link(8, "a1", "u1"),
            ])
            .expect("encode"),
            "erase-aliases",
        )
        .expect("aliases");
    let traits = |site, user: &str| TraitsRow {
        site_id: site,
        user_id: user.to_owned(),
        traits_str: vec![("plan".to_owned(), "pro".to_owned())],
        traits_num: vec![],
        updated_ms: t,
    };
    scratch
        .store
        .insert_traits_lines(
            &Store::encode_traits(&[traits(7, "u1"), traits(7, "u2"), traits(8, "u1")])
                .expect("encode"),
            "erase-traits",
        )
        .expect("traits");
}

#[test]
#[ignore = "needs ClickHouse"]
fn erasing_a_user_hides_the_rows_at_once_and_removes_them_when_the_mask_is_applied() {
    let scratch = Scratch::new();
    seed_two_users(&scratch);
    let store = &scratch.store;

    let anons = store.linked_anon_ids(7, "u1").expect("linked");
    assert_eq!(anons, vec!["a1", "a2"]);
    assert_eq!(
        store.linked_anon_ids(7, "nobody").expect("none"),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &store
                .count_user_rows(7, Some("u1"), &anons, false)
                .expect("before")
        ),
        vec![
            ("events", 3),
            ("identity_alias", 3),
            ("user_traits", 1),
            ("sessions", 2),
            ("errors", 0),
            ("measurements", 0)
        ],
        "a1 twice and a2 once; both links of a2 and the link of a1; the traits"
    );

    store
        .delete_user_rows(7, Some("u1"), &anons)
        .expect("delete");
    assert_eq!(
        rows(
            &store
                .count_user_rows(7, Some("u1"), &anons, false)
                .expect("seen")
        ),
        vec![
            ("events", 0),
            ("identity_alias", 0),
            ("user_traits", 0),
            ("sessions", 0),
            ("errors", 0),
            ("measurements", 0)
        ],
        "queries no longer see the rows"
    );
    // Whether the data is still in the parts here depends on the version: 26.8
    // keeps it until the mask is applied, 25.8 has already rewritten the parts.
    // Only that it never exceeds what was there can be asserted.
    let on_disk = rows(
        &store
            .count_user_rows(7, Some("u1"), &anons, true)
            .expect("disk"),
    );
    assert!(
        on_disk
            .iter()
            .zip([3, 3, 1])
            .all(|((_, found), before)| *found <= before),
        "{on_disk:?}"
    );

    let needed = store
        .rewrites_needed(7, Some("u1"), &anons)
        .expect("what to rewrite");
    store.apply_rewrites(&needed).expect("apply the mask");
    assert_eq!(
        rows(
            &store
                .count_user_rows(7, Some("u1"), &anons, true)
                .expect("gone")
        ),
        vec![
            ("events", 0),
            ("identity_alias", 0),
            ("user_traits", 0),
            ("sessions", 0),
            ("errors", 0),
            ("measurements", 0)
        ],
        "now it is gone from disk"
    );

    // What belongs to anyone else is untouched.
    assert_eq!(count(store, "events"), 2, "a3 on site 7 and a1 on site 8");
    assert_eq!(
        count(store, "identity_alias"),
        2,
        "u2 on a3, and u1 on site 8; the link of u2 to a2 went with the device"
    );
    assert_eq!(count(store, "user_traits"), 2);
    assert_eq!(
        store.linked_anon_ids(8, "u1").expect("site 8"),
        vec!["a1"],
        "the same names on another site"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn erasing_a_user_with_no_data_and_erasing_twice_are_harmless() {
    let scratch = Scratch::new();
    seed_two_users(&scratch);
    let store = &scratch.store;
    store
        .delete_user_rows(7, Some("ghost"), &[])
        .expect("nothing to delete");
    let nothing = store
        .rewrites_needed(7, Some("ghost"), &[])
        .expect("what to rewrite");
    assert!(nothing.is_empty(), "{nothing:?}");
    store.apply_rewrites(&nothing).expect("nothing to rewrite");
    assert_eq!(count(store, "events"), 5);

    let anons = store.linked_anon_ids(7, "u2").expect("linked");
    for _ in 0..2 {
        store
            .delete_user_rows(7, Some("u2"), &anons)
            .expect("delete");
        let needed = store
            .rewrites_needed(7, Some("u2"), &anons)
            .expect("needed");
        store.apply_rewrites(&needed).expect("apply");
    }
    assert_eq!(
        rows(
            &store
                .count_user_rows(7, Some("u2"), &anons, true)
                .expect("gone")
        ),
        vec![
            ("events", 0),
            ("identity_alias", 0),
            ("user_traits", 0),
            ("sessions", 0),
            ("errors", 0),
            ("measurements", 0)
        ]
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn an_identifier_that_could_alter_the_statement_never_reaches_clickhouse() {
    let scratch = Scratch::new();
    seed_two_users(&scratch);
    let store = &scratch.store;
    for bad in ["x' OR 1 = 1 --", "a;DROP TABLE events", "", "a b"] {
        assert!(store.linked_anon_ids(7, bad).is_err(), "{bad:?}");
        assert!(
            store.delete_user_rows(7, Some(bad), &[]).is_err(),
            "{bad:?}"
        );
        assert!(
            store
                .delete_user_rows(7, Some("u1"), &[bad.to_owned()])
                .is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(count(store, "events"), 5, "nothing was deleted");
}

#[test]
#[ignore = "needs ClickHouse"]
fn only_the_partitions_that_hold_the_erased_rows_are_rewritten() {
    use dionaea_store::Rewrite;

    let scratch = Scratch::new();
    let store = &scratch.store;
    // September, October and November 2025.
    let (sep, oct, nov) = (1_757_000_000_000, 1_760_000_000_000, 1_763_000_000_000);
    let mut events = Vec::new();
    for (n, anon, ts) in [
        (1, "a1", sep),
        (2, "a1", nov),
        (3, "a2", oct),
        (4, "a2", nov),
    ] {
        let mut row = event(n);
        row.anon_id = anon.to_owned();
        row.ts_ms = ts;
        row.received_ms = now_ms();
        events.push(row);
    }
    store.insert_events(&events, "partitions").expect("events");
    store
        .insert_alias_lines(
            &Store::encode_aliases(&[alias("a1", "u1", now_ms()), alias("a2", "u2", now_ms())])
                .expect("encode"),
            "partitions-links",
        )
        .expect("links");

    let anons = vec!["a1".to_owned()];
    store
        .delete_user_rows(7, Some("u1"), &anons)
        .expect("delete");
    let needed = store
        .rewrites_needed(7, Some("u1"), &anons)
        .expect("needed");
    // Whether the rows are still in the parts depends on the version, but
    // never more than the months of the erased device, and never the month
    // that only the other person has.
    let events_partitions: Vec<Option<u32>> = needed
        .iter()
        .filter(|r| r.table == "events")
        .map(|r| r.partition)
        .collect();
    assert!(
        events_partitions
            .iter()
            .all(|p| matches!(p, Some(202_509 | 202_511))),
        "{needed:?}"
    );
    assert!(
        needed
            .iter()
            .all(|r| r.table != "events" || r.partition.is_some())
    );
    // The links table is not partitioned: the whole table, or nothing.
    assert!(
        needed
            .iter()
            .filter(|r| r.table == "identity_alias")
            .all(|r| *r
                == Rewrite {
                    table: "identity_alias",
                    partition: None
                })
    );

    store.apply_rewrites(&needed).expect("rewrite");
    store
        .apply_rewrites(&needed)
        .expect("the same again is harmless");
    assert_eq!(
        rows(
            &store
                .count_user_rows(7, Some("u1"), &anons, true)
                .expect("gone")
        ),
        vec![
            ("events", 0),
            ("identity_alias", 0),
            ("user_traits", 0),
            ("sessions", 0),
            ("errors", 0),
            ("measurements", 0)
        ]
    );
    assert_eq!(count(store, "events"), 2, "the other person's two events");
    assert_eq!(count(store, "identity_alias"), 1);

    assert!(
        store
            .apply_rewrites(&[Rewrite {
                table: "_migrations",
                partition: None
            }])
            .is_err(),
        "only identifier tables"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn erasing_a_device_alone_removes_what_is_under_it_and_leaves_the_users_traits() {
    let scratch = Scratch::new();
    seed_two_users(&scratch);
    let store = &scratch.store;
    let device = vec!["a2".to_owned()];

    assert_eq!(
        rows(
            &store
                .count_user_rows(7, None, &device, false)
                .expect("before")
        ),
        vec![
            ("events", 1),
            ("identity_alias", 2),
            ("user_traits", 0),
            ("sessions", 1),
            ("errors", 0),
            ("measurements", 0)
        ],
        "the event of a2 and both links to it; no user, so no traits"
    );
    store.delete_user_rows(7, None, &device).expect("delete");
    let needed = store.rewrites_needed(7, None, &device).expect("needed");
    store.apply_rewrites(&needed).expect("rewrite");
    assert_eq!(
        rows(&store.count_user_rows(7, None, &device, true).expect("gone")),
        vec![
            ("events", 0),
            ("identity_alias", 0),
            ("user_traits", 0),
            ("sessions", 0),
            ("errors", 0),
            ("measurements", 0)
        ]
    );
    // Everything else stays: the other events, the other links, all traits.
    assert_eq!(count(store, "events"), 4);
    assert_eq!(count(store, "identity_alias"), 3);
    assert_eq!(count(store, "user_traits"), 3);
}

// Sessions.

fn session_event(n: u64, session: &str, anon: &str, screen: &str, ts: u64) -> EventRow {
    let mut row = event(n);
    row.session_id = session.to_owned();
    row.anon_id = anon.to_owned();
    row.screen = screen.to_owned();
    row.ts_ms = ts;
    row
}

const T0: u64 = 1_760_000_000_000;
const MINUTE: u64 = 60_000;

#[test]
#[ignore = "needs ClickHouse"]
fn events_with_a_session_id_are_summarized_even_when_they_arrive_in_separate_inserts() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                session_event(1, "s1", "a1", "/home", T0),
                session_event(2, "s1", "a1", "/cart", T0 + 5 * MINUTE),
                session_event(3, "s2", "a2", "/home", T0 + MINUTE),
                // No session id: not part of the aggregate.
                session_event(4, "", "a3", "/home", T0),
            ],
            "sessions-1",
        )
        .expect("first insert");
    store
        .insert_events(
            &[session_event(5, "s1", "a1", "/pay", T0 + 9 * MINUTE)],
            "sessions-2",
        )
        .expect("second insert");

    let rows = store
        .raw_query(
            "SELECT session_id, anon_id, toString(min(first_ts)), toString(max(last_ts)),
                    sum(events), argMinMerge(entry_screen), argMaxMerge(exit_screen),
                    uniqMerge(screens), argMinMerge(platform)
             FROM sessions GROUP BY site_id, session_id, anon_id
             ORDER BY session_id FORMAT TabSeparated",
        )
        .expect("sessions");
    let lines: Vec<Vec<&str>> = rows.lines().map(|l| l.split('\t').collect()).collect();
    assert_eq!(lines.len(), 2, "two sessions, not three: {rows}");
    // s1: three events over nine minutes, in two inserts.
    assert_eq!(lines[0][..2], ["s1", "a1"]);
    assert_eq!(lines[0][4], "3");
    assert_eq!(
        (lines[0][5], lines[0][6], lines[0][7], lines[0][8]),
        ("/home", "/pay", "3", "ios")
    );
    let first: u64 = scratch
        .admin
        .raw_query(&format!(
            "SELECT toUnixTimestamp64Milli(toDateTime64('{}', 3, 'UTC'))",
            lines[0][2]
        ))
        .expect("time")
        .trim()
        .parse()
        .expect("ms");
    let last: u64 = scratch
        .admin
        .raw_query(&format!(
            "SELECT toUnixTimestamp64Milli(toDateTime64('{}', 3, 'UTC'))",
            lines[0][3]
        ))
        .expect("time")
        .trim()
        .parse()
        .expect("ms");
    assert_eq!((first, last), (T0, T0 + 9 * MINUTE));
    assert_eq!(lines[1][..2], ["s2", "a2"]);
    assert_eq!(lines[1][4], "1", "a bounce: one event");
}

#[test]
#[ignore = "needs ClickHouse"]
fn events_without_a_session_id_are_split_by_thirty_minutes_of_inactivity_at_query_time() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                session_event(1, "", "h1", "/a", T0),
                session_event(2, "", "h1", "/b", T0 + 10 * MINUTE),
                // 50 minutes later: a new session of the same device.
                session_event(3, "", "h1", "/c", T0 + 60 * MINUTE),
                session_event(4, "", "h2", "/a", T0 + 61 * MINUTE),
                // Exactly 30 minutes after the previous event is still the same.
                session_event(5, "", "h3", "/a", T0),
                session_event(6, "", "h3", "/b", T0 + 30 * MINUTE),
                // One with an id of its own is left to the aggregate.
                session_event(7, "own", "h4", "/a", T0),
            ],
            "derived",
        )
        .expect("insert");
    let rows = store
        .raw_query(
            "SELECT anon_id, session_number, events, entry_screen, exit_screen
             FROM derived_sessions ORDER BY anon_id, session_number FORMAT TabSeparated",
        )
        .expect("derived");
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        vec![
            "h1\t1\t2\t/a\t/b",
            "h1\t2\t1\t/c\t/c",
            "h2\t1\t1\t/a\t/a",
            "h3\t1\t2\t/a\t/b",
        ],
        "{rows}"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_ingest_user_fills_the_sessions_by_inserting_events_and_can_read_neither() {
    let scratch = Scratch::new();
    let roles = roles("sessions");
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
        .insert_events(
            &[session_event(1, "s1", "a1", "/home", T0)],
            "ingest-sessions",
        )
        .expect("the ingest user inserts events");
    assert_eq!(
        count(&scratch.store, "sessions"),
        1,
        "and the view followed"
    );
    for sql in ["SELECT count() FROM sessions", "SELECT count() FROM events"] {
        let error = ingest.raw_query(sql).expect_err(sql);
        assert!(denied(&error), "{sql}: {error:?}");
    }
    let query = user_store(&scratch.database, &roles.query_user, &roles.query_password);
    assert_eq!(count(&query, "sessions"), 1, "the query user reads them");
}

#[test]
#[ignore = "needs ClickHouse"]
fn erasing_a_device_removes_its_sessions_too() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                session_event(1, "s1", "a1", "/home", T0),
                session_event(2, "s2", "a2", "/home", T0),
            ],
            "erase-sessions",
        )
        .expect("insert");
    assert_eq!(count(store, "sessions"), 2);
    let devices = vec!["a1".to_owned()];
    store.delete_user_rows(7, None, &devices).expect("delete");
    let needed = store.rewrites_needed(7, None, &devices).expect("needed");
    store.apply_rewrites(&needed).expect("rewrite");
    assert_eq!(count(store, "sessions"), 1);
    assert_eq!(
        store
            .raw_query("SELECT anon_id FROM sessions SETTINGS apply_deleted_mask = 0")
            .expect("left")
            .trim(),
        "a2",
        "and nothing of a1 is left on disk"
    );
}

// Retention.

fn at(day: &str) -> u64 {
    // Midnight UTC of a day in 2025, in milliseconds.
    let days: u64 = match day {
        "08-01" => 20_301,
        "09-10" => 20_341,
        "09-20" => 20_351,
        "10-01" => 20_362,
        "10-15" => 20_376,
        "10-20" => 20_381,
        "11-01" => 20_393,
        other => panic!("no such day {other}"),
    };
    days * 86_400_000
}

fn dated(n: u64, site: u32, session: &str, day: &str) -> EventRow {
    let mut row = session_event(n, session, "a1", "/home", at(day));
    row.site_id = site;
    row.received_ms = now_ms();
    row
}

#[test]
#[ignore = "needs ClickHouse"]
fn each_site_loses_only_what_is_older_than_its_own_cutoff() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                dated(1, 7, "s1", "08-01"),
                dated(2, 7, "s2", "10-01"),
                dated(3, 7, "s3", "11-01"),
                dated(4, 8, "s4", "08-01"),
                dated(5, 8, "s5", "09-20"),
                dated(6, 8, "s6", "10-20"),
            ],
            "retention",
        )
        .expect("events");
    assert_eq!(count(store, "events"), 6);
    assert_eq!(count(store, "sessions"), 6);

    // Site 7 keeps data from the 15th of October, site 8 from the 10th of
    // September.
    let seven = store.expire_analytics(7, at("10-15")).expect("site 7");
    assert_eq!(
        seven,
        vec![
            Expired {
                table: "events",
                rows: 2
            },
            Expired {
                table: "sessions",
                rows: 2
            },
            Expired {
                table: "errors",
                rows: 0
            },
            Expired {
                table: "measurements",
                rows: 0
            }
        ]
    );
    let eight = store.expire_analytics(8, at("09-10")).expect("site 8");
    assert_eq!(
        eight[0],
        Expired {
            table: "events",
            rows: 1
        }
    );
    assert_eq!(
        store.expire_analytics(8, at("09-10")).expect("again")[0].rows,
        0,
        "a second run finds nothing"
    );
    assert_eq!(count(store, "events"), 3, "queries see what is kept");

    // The oldest cutoff drops whole months; the newest tells what to rewrite.
    let dropped = store.drop_expired_partitions(at("09-10")).expect("drop");
    assert!(
        dropped.contains(&("events", 202_508)) && dropped.contains(&("sessions", 202_508)),
        "{dropped:?}"
    );
    assert!(
        dropped.iter().all(|(_, partition)| *partition == 202_508),
        "{dropped:?}"
    );
    let rewritten = store.rewrite_expired(at("10-15")).expect("rewrite");
    assert!(
        rewritten
            .iter()
            .all(|r| matches!(r.partition, Some(202_509 | 202_510))),
        "{rewritten:?}"
    );

    // Nothing that expired is left on disk, and what is kept is all there.
    let physical = |table: &str| -> u64 {
        store
            .raw_query(&format!(
                "SELECT count() FROM {table} SETTINGS apply_deleted_mask = 0"
            ))
            .expect("count")
            .trim()
            .parse()
            .expect("number")
    };
    assert_eq!(physical("events"), 3);
    assert_eq!(physical("sessions"), 3);
    assert_eq!(
        store
            .raw_query("SELECT site_id, toString(toDate(ts)) FROM events ORDER BY site_id, ts FORMAT TabSeparated")
            .expect("kept")
            .lines()
            .collect::<Vec<_>>(),
        vec!["7\t2025-11-01", "8\t2025-09-20", "8\t2025-10-20"]
    );
    assert!(
        store
            .rewrite_expired(at("10-15"))
            .expect("again")
            .is_empty(),
        "nothing left to rewrite"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_site_with_nothing_old_loses_nothing_and_a_partition_someone_keeps_is_not_dropped() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[dated(1, 7, "s1", "09-20"), dated(2, 8, "s2", "09-20")],
            "keep",
        )
        .expect("events");
    let nothing = store.expire_analytics(7, at("08-01")).expect("expire");
    assert!(nothing.iter().all(|e| e.rows == 0));
    // The month of September is not over before the cutoff of the 10th.
    assert!(
        store
            .drop_expired_partitions(at("09-10"))
            .expect("drop")
            .is_empty()
    );
    assert_eq!(count(store, "events"), 2);
    // Dropping with a cutoff after the month removes it for everyone.
    let dropped = store.drop_expired_partitions(at("10-15")).expect("drop");
    assert!(dropped.contains(&("events", 202_509)), "{dropped:?}");
    assert_eq!(count(store, "events"), 0);
}

#[test]
#[ignore = "needs ClickHouse"]
fn events_no_longer_expire_by_a_rule_of_the_table_itself() {
    let scratch = Scratch::new();
    let ddl = scratch
        .store
        .raw_query("SHOW CREATE TABLE events")
        .expect("ddl");
    assert!(
        ddl.contains("INTERVAL 60 YEAR") || ddl.contains("toIntervalYear(60)"),
        "{ddl}"
    );
    assert!(
        !ddl.contains("13 MONTH") && !ddl.contains("toIntervalMonth(13)"),
        "{ddl}"
    );
    // An event as old as the first one is not removed by a merge.
    let mut row = dated(1, 7, "s1", "08-01");
    row.ts_ms = 1_000_000_000_000; // 2001
    scratch.store.insert_events(&[row], "old").expect("insert");
    scratch
        .store
        .raw_query("OPTIMIZE TABLE events FINAL")
        .expect("merge");
    assert_eq!(count(&scratch.store, "events"), 1);
}

// Errors.

fn error_event(n: u64, anon: &str, fingerprint: &str, ts: u64) -> EventRow {
    let mut row = event(n);
    row.name = "app_error".to_owned();
    row.anon_id = anon.to_owned();
    row.ts_ms = ts;
    row.props_str = [
        ("kind", "unhandled"),
        ("type", "TypeError"),
        ("message", "bad quantity"),
        ("frames", "src/util.js|parseQuantity|4|11|1"),
        ("symbols", "ok"),
        ("fingerprint", fingerprint),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .to_vec();
    row
}

#[test]
#[ignore = "needs ClickHouse"]
fn error_events_are_copied_into_the_errors_table_and_other_events_are_not() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                event(1),
                error_event(2, "a1", "f1", T0),
                error_event(3, "a2", "f2", T0),
            ],
            "errors-1",
        )
        .expect("insert");
    assert_eq!(count(store, "events"), 3);
    assert_eq!(count(store, "errors"), 2);
    let rows = store
        .raw_query(
            "SELECT anon_id, fingerprint, symbols, kind, error_type, message, frames \
             FROM errors ORDER BY anon_id FORMAT TabSeparated",
        )
        .expect("errors");
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        vec![
            "a1\tf1\tok\tunhandled\tTypeError\tbad quantity\tsrc/util.js|parseQuantity|4|11|1",
            "a2\tf2\tok\tunhandled\tTypeError\tbad quantity\tsrc/util.js|parseQuantity|4|11|1",
        ]
    );
    // Another insert of the same error is another row: only a retried batch is dropped.
    store
        .insert_events(&[error_event(2, "a1", "f1", T0)], "errors-2")
        .expect("insert");
    assert_eq!(count(store, "errors"), 3, "a new token stores a new row");
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_retried_batch_does_not_repeat_its_errors() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let batch = [error_event(1, "a1", "f1", T0)];
    store.insert_events(&batch, "same").expect("first");
    store.insert_events(&batch, "same").expect("retry");
    assert_eq!(count(store, "events"), 1);
    assert_eq!(count(store, "errors"), 1);
}

#[test]
#[ignore = "needs ClickHouse"]
fn erasing_a_device_removes_its_errors_too() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                error_event(1, "a1", "f1", T0),
                error_event(2, "a2", "f1", T0),
            ],
            "erase-errors",
        )
        .expect("insert");
    assert_eq!(count(store, "errors"), 2);
    let devices = vec!["a1".to_owned()];
    store.delete_user_rows(7, None, &devices).expect("delete");
    let needed = store.rewrites_needed(7, None, &devices).expect("needed");
    store.apply_rewrites(&needed).expect("rewrite");
    assert_eq!(count(store, "errors"), 1);
    assert_eq!(
        store
            .raw_query("SELECT anon_id FROM errors SETTINGS apply_deleted_mask = 0")
            .expect("left")
            .trim(),
        "a2",
        "and nothing of a1 is left on disk"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_sites_retention_reaches_its_errors() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let mut old = error_event(1, "a1", "f1", at("08-01"));
    let mut recent = error_event(2, "a1", "f1", at("10-20"));
    for row in [&mut old, &mut recent] {
        row.received_ms = now_ms();
    }
    store
        .insert_events(&[old, recent], "retention-errors")
        .expect("insert");
    assert_eq!(count(store, "errors"), 2);
    let expired = store.expire_analytics(7, at("10-15")).expect("expire");
    assert!(
        expired.contains(&Expired {
            table: "errors",
            rows: 1
        }),
        "{expired:?}"
    );
    assert_eq!(count(store, "errors"), 1);
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_ingest_user_fills_the_errors_by_inserting_events_and_cannot_read_them() {
    let scratch = Scratch::new();
    let roles = roles("errors");
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
        .insert_events(&[error_event(1, "a1", "f1", T0)], "ingest-errors")
        .expect("the ingest user inserts events");
    assert_eq!(count(&scratch.store, "errors"), 1, "and the view followed");
    let error = ingest
        .raw_query("SELECT count() FROM errors")
        .expect_err("denied");
    assert!(denied(&error), "{error:?}");
    let query = user_store(&scratch.database, &roles.query_user, &roles.query_password);
    assert_eq!(count(&query, "errors"), 1, "the query user reads them");
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_retried_batch_does_not_count_its_events_twice_in_sessions() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let batch = [session_event(1, "s1", "a1", "/home", T0)];
    store.insert_events(&batch, "same").expect("first");
    store.insert_events(&batch, "same").expect("retry");
    let events = store
        .raw_query("SELECT sum(events) FROM sessions FORMAT TabSeparated")
        .expect("sessions");
    assert_eq!(events.trim(), "1");
}

// Measurements and the health rollups.

fn health_event(n: u64, name: &str, strs: &[(&str, &str)], nums: &[(&str, f64)]) -> EventRow {
    let mut row = event(n);
    row.name = name.to_owned();
    row.props_str = strs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    row.props_num = nums.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
    row
}

#[test]
#[ignore = "needs ClickHouse"]
fn health_events_become_one_measurement_per_value() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                event(1),
                health_event(
                    2,
                    "app_start",
                    &[("start", "cold")],
                    &[("first_frame", 800.0), ("interactive", 1500.0)],
                ),
                // Optional value absent: one row, not two.
                health_event(
                    3,
                    "app_start",
                    &[("start", "warm")],
                    &[("first_frame", 300.0)],
                ),
                health_event(4, "screen_load", &[], &[("render", 120.0)]),
                health_event(
                    5,
                    "http_request",
                    &[
                        ("method", "post"),
                        ("host", "api.example.com"),
                        ("route", "/orders/:id"),
                        ("connection", "wifi"),
                    ],
                    &[("status", 503.0), ("duration", 250.0)],
                ),
                health_event(
                    6,
                    "web_vital",
                    &[("name", "lcp"), ("rating", "good"), ("route", "/home")],
                    &[("value", 1900.0)],
                ),
            ],
            "measure-1",
        )
        .expect("insert");
    assert_eq!(count(store, "events"), 6);
    let rows = store
        .raw_query(
            "SELECT anon_id, metric, value, sample_rate, detail, route, host, status, connection \
             FROM measurements ORDER BY anon_id, metric FORMAT TabSeparated",
        )
        .expect("measurements");
    assert_eq!(
        rows.lines().collect::<Vec<_>>(),
        vec![
            "a2\tapp_start_first_frame\t800\t1\tcold\t\t\t0\t",
            "a2\tapp_start_interactive\t1500\t1\tcold\t\t\t0\t",
            "a3\tapp_start_first_frame\t300\t1\twarm\t\t\t0\t",
            "a4\tscreen_load_render\t120\t1\t\t\t\t0\t",
            "a5\thttp_duration\t250\t1\tpost\t/orders/:id\tapi.example.com\t503\twifi",
            "a6\tweb_vital_lcp\t1900\t1\tgood\t/home\t\t0\t",
        ]
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_retried_batch_does_not_repeat_its_measurements() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let batch = [health_event(
        1,
        "app_start",
        &[("start", "cold")],
        &[("first_frame", 800.0), ("interactive", 1500.0)],
    )];
    store.insert_events(&batch, "same").expect("first");
    store.insert_events(&batch, "same").expect("retry");
    assert_eq!(count(store, "measurements"), 2);
    let samples = store
        .raw_query("SELECT sum(samples) FROM perf_daily")
        .expect("perf");
    assert_eq!(samples.trim(), "2");
}

#[test]
#[ignore = "needs ClickHouse"]
fn perf_daily_keeps_percentiles_by_target_release_and_day() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let request = |n: u64| {
        health_event(
            n,
            "http_request",
            &[("method", "get"), ("host", "h"), ("route", "/a")],
            &[("status", 200.0), ("duration", n as f64 * 10.0)],
        )
    };
    let rows: Vec<EventRow> = (1..=100).map(request).collect();
    store.insert_events(&rows, "perf-1").expect("insert");
    // Two blocks, one key: the states merge at read time.
    store
        .insert_events(&[request(101)], "perf-2")
        .expect("insert");
    let line = store
        .raw_query(
            "SELECT target, sum(samples), \
                    arrayMap(x -> round(x / 10), quantilesTDigestMerge(0.5, 0.75, 0.9, 0.95, 0.99)(quantiles)) \
             FROM perf_daily WHERE metric = 'http_duration' GROUP BY target FORMAT TabSeparated",
        )
        .expect("percentiles");
    let fields: Vec<&str> = line.trim().split('\t').collect();
    assert_eq!(fields[..2], ["/a", "101"], "{line}");
    // Values 10 to 1010 in steps of 10: the median is 510 (51 in tens).
    assert!(fields[2].starts_with("[51,"), "{line}");
}

#[test]
#[ignore = "needs ClickHouse"]
fn issues_daily_counts_errors_per_fingerprint_release_and_day() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let mut other_release = error_event(3, "a3", "f1", T0 + 5_000);
    other_release.app_version = "1.5.0".to_owned();
    store
        .insert_events(
            &[
                error_event(1, "a1", "f1", T0),
                error_event(2, "a2", "f1", T0 + 1_000),
                other_release,
                error_event(4, "a4", "f2", T0),
            ],
            "issues-1",
        )
        .expect("insert");
    store
        .insert_events(&[error_event(5, "a5", "f1", T0 + 2_000)], "issues-2")
        .expect("insert");
    let rows = store
        .raw_query(
            "SELECT fingerprint, app_version, sum(events), toUnixTimestamp64Milli(min(first_ts)), \
                    toUnixTimestamp64Milli(max(last_ts)) \
             FROM issues_daily GROUP BY fingerprint, app_version ORDER BY fingerprint, app_version \
             FORMAT TabSeparated",
        )
        .expect("issues");
    assert_eq!(
        rows.lines().map(str::to_owned).collect::<Vec<_>>(),
        vec![
            format!("f1\t1.4.2+57\t3\t{}\t{}", T0, T0 + 2_000),
            format!("f1\t1.5.0\t1\t{}\t{}", T0 + 5_000, T0 + 5_000),
            format!("f2\t1.4.2+57\t1\t{}\t{}", T0, T0),
        ]
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn erasing_a_device_removes_its_measurements_and_leaves_the_rollups() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                health_event(1, "screen_load", &[], &[("render", 100.0)]),
                health_event(2, "screen_load", &[], &[("render", 200.0)]),
            ],
            "erase-measure",
        )
        .expect("insert");
    assert_eq!(count(store, "measurements"), 2);
    let devices = vec!["a1".to_owned()];
    store.delete_user_rows(7, None, &devices).expect("delete");
    let needed = store.rewrites_needed(7, None, &devices).expect("needed");
    store.apply_rewrites(&needed).expect("rewrite");
    assert_eq!(count(store, "measurements"), 1);
    assert_eq!(
        store
            .raw_query("SELECT anon_id FROM measurements SETTINGS apply_deleted_mask = 0")
            .expect("left")
            .trim(),
        "a2"
    );
    // The rollup keeps the aggregate, with no way back to the device.
    assert_eq!(
        store
            .raw_query("SELECT sum(samples) FROM perf_daily")
            .expect("perf")
            .trim(),
        "2"
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn release_lookups_find_the_releases_of_an_error_and_when_a_release_began() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let mut old = error_event(1, "a1", "f1", T0);
    old.received_ms = T0 + 1_000;
    let mut late = error_event(2, "a2", "f1", T0 + 10_000);
    late.app_version = "1.5.0".to_owned();
    late.received_ms = T0 + 20_000;
    let mut none = error_event(3, "a3", "f1", T0 + 11_000);
    none.app_version = String::new();
    none.received_ms = T0 + 21_000;
    store
        .insert_events(&[old, late, none], "lookups")
        .expect("insert");
    let names = |sightings: Vec<dionaea_store::Sighting>| {
        sightings
            .into_iter()
            .map(|s| (s.release, s.first_ms))
            .collect::<Vec<_>>()
    };
    let all = names(store.error_releases(7, "f1", 0, "").expect("releases"));
    assert_eq!(
        all,
        [
            ("1.4.2+57".to_owned(), Some((T0 + 1_000) as i64)),
            ("1.5.0".to_owned(), Some((T0 + 20_000) as i64)),
        ],
        "the empty label is left out, and each release says when it began"
    );
    let since = names(
        store
            .error_releases(7, "f1", (T0 + 15_000) as i64, "")
            .expect("since"),
    );
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].0, "1.5.0");
    let after = names(store.error_releases(7, "f1", 0, "1.4.2+57").expect("after"));
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, "1.5.0");
    assert!(
        store
            .error_releases(7, "nope", 0, "")
            .expect("none")
            .is_empty()
    );
    assert!(
        store
            .error_releases(8, "f1", 0, "")
            .expect("site")
            .is_empty()
    );
    assert_eq!(
        store.release_first_seen(7, "1.5.0").expect("first"),
        Some((T0 + 20_000) as i64)
    );
    assert_eq!(store.release_first_seen(7, "9.9").expect("none"), None);
    assert_eq!(
        store.release_first_seen(8, "1.5.0").expect("other site"),
        None
    );
    // A value that looks like SQL is only a value.
    assert_eq!(
        store.release_first_seen(7, "x' OR 1=1 --").expect("nasty"),
        None
    );
    assert!(
        store
            .error_releases(7, "x' OR 1=1 --", 0, "x' OR 1=1 --")
            .expect("nasty")
            .is_empty()
    );
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_release_keeps_its_first_sight_across_inserts_and_after_its_events_expire() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let mut first = event(1);
    first.received_ms = T0;
    let mut second = event(2);
    second.received_ms = T0 + 60_000;
    store.insert_events(&[second], "first-b").expect("insert");
    store.insert_events(&[first], "first-a").expect("insert");
    assert_eq!(
        store.release_first_seen(7, "1.4.2+57").expect("first"),
        Some(T0 as i64),
        "the earliest wins whatever the order of arrival"
    );
    // The events go (retention); the release remembers when it began.
    store
        .expire_analytics(7, T0 + 10 * 86_400_000)
        .expect("expire");
    store
        .raw_query("OPTIMIZE TABLE releases FINAL")
        .expect("merge");
    assert_eq!(
        store.release_first_seen(7, "1.4.2+57").expect("kept"),
        Some(T0 as i64)
    );
    assert_eq!(count(store, "events"), 0);
}

#[test]
#[ignore = "needs ClickHouse"]
fn error_releases_come_in_pages_in_name_order_without_leaving_any_out() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let total = dionaea_store::RELEASES_PER_PAGE as u64 + 20;
    let rows: Vec<EventRow> = (0..total)
        .map(|n| {
            let mut row = error_event(n + 1, "a1", "f1", T0 + n);
            row.app_version = format!("1.0.{n:05}");
            row
        })
        .collect();
    store.insert_events(&rows, "pages").expect("insert");
    let mut seen = Vec::new();
    let mut after = String::new();
    loop {
        let page = store.error_releases(7, "f1", 0, &after).expect("page");
        let last = page.len() < dionaea_store::RELEASES_PER_PAGE as usize;
        if let Some(end) = page.last() {
            after = end.release.clone();
        }
        seen.extend(page.into_iter().map(|s| s.release));
        if last {
            break;
        }
    }
    assert_eq!(seen.len() as u64, total);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, seen, "in name order and once each");
}

#[test]
#[ignore = "needs ClickHouse"]
fn running_the_migration_again_copies_older_events_once() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                health_event(1, "screen_load", &[], &[("render", 100.0)]),
                health_event(
                    2,
                    "app_start",
                    &[("start", "cold")],
                    &[("first_frame", 800.0), ("interactive", 1500.0)],
                ),
                error_event(3, "a3", "f1", T0),
                error_event(4, "a4", "f1", T0 + 1_000),
            ],
            "backfill",
        )
        .expect("insert");
    let snapshot = || {
        store
            .raw_query(
                "SELECT
                   (SELECT count() FROM measurements),
                   (SELECT sum(samples) FROM perf_daily),
                   (SELECT sum(events) FROM issues_daily),
                   (SELECT uniqExact(app_version) FROM releases),
                   (SELECT toString(min(first_seen)) FROM releases)
                 FORMAT TabSeparated",
            )
            .expect("snapshot")
    };
    let live = snapshot();
    assert!(live.starts_with("3\t3\t2\t1\t"), "{live}");

    // As if the tables were new and the events older: empty the copies and
    // run the migration again.
    for table in ["measurements", "perf_daily", "issues_daily", "releases"] {
        scratch
            .store
            .raw_query(&format!("TRUNCATE TABLE {table}"))
            .expect("truncate");
    }
    scratch
        .store
        .raw_query("ALTER TABLE _migrations DELETE WHERE version = 8 SETTINGS mutations_sync = 2")
        .expect("forget the migration");
    scratch.store.migrate().expect("migrate again");
    assert_eq!(snapshot(), live, "everything the events imply is back");

    // And again: nothing is counted twice.
    scratch
        .store
        .raw_query("ALTER TABLE _migrations DELETE WHERE version = 8 SETTINGS mutations_sync = 2")
        .expect("forget the migration");
    scratch.store.migrate().expect("migrate a third time");
    assert_eq!(snapshot(), live, "a second run changes nothing");
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_retried_batch_does_not_repeat_its_errors_in_issues_daily() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let batch = [
        error_event(1, "a1", "f1", T0),
        error_event(2, "a2", "f1", T0),
    ];
    store.insert_events(&batch, "same").expect("first");
    store.insert_events(&batch, "same").expect("retry");
    let events = store
        .raw_query("SELECT sum(events) FROM issues_daily FORMAT TabSeparated")
        .expect("issues");
    assert_eq!(events.trim(), "2");
}

#[test]
#[ignore = "needs ClickHouse"]
fn the_error_count_of_an_issue_follows_the_time_and_the_site() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    let mut early = error_event(1, "a1", "f1", T0);
    early.received_ms = T0;
    let mut late = error_event(2, "a2", "f1", T0 + 10_000);
    late.received_ms = T0 + 10_000;
    store
        .insert_events(&[early, late, error_event(3, "a3", "f2", T0)], "counts")
        .expect("insert");
    assert_eq!(store.error_count(7, "f1", 0).expect("all"), 2);
    assert_eq!(
        store
            .error_count(7, "f1", (T0 + 5_000) as i64)
            .expect("since"),
        1
    );
    assert_eq!(store.error_count(7, "nope", 0).expect("none"), 0);
    assert_eq!(store.error_count(8, "f1", 0).expect("site"), 0);
    assert_eq!(store.error_count(7, "x' OR 1=1 --", 0).expect("nasty"), 0);
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_sampled_event_carries_its_rate_into_the_measurement_and_the_weight() {
    let scratch = Scratch::new();
    let store = &scratch.store;
    store
        .insert_events(
            &[
                health_event(
                    1,
                    "screen_load",
                    &[],
                    &[("render", 100.0), ("sample_rate", 0.25)],
                ),
                health_event(2, "screen_load", &[], &[("render", 200.0)]),
            ],
            "sampled",
        )
        .expect("insert");
    let rows = store
        .raw_query(
            "SELECT anon_id, sample_rate FROM measurements ORDER BY anon_id FORMAT TabSeparated",
        )
        .expect("rates");
    assert_eq!(rows.lines().collect::<Vec<_>>(), ["a1\t0.25", "a2\t1"]);
    // One event standing for four, and one for itself: five in all.
    let weight = store
        .raw_query("SELECT sum(samples), sum(weight) FROM perf_daily FORMAT TabSeparated")
        .expect("weight");
    assert_eq!(weight.trim(), "2\t5");
}
