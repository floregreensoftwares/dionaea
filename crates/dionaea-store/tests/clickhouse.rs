//! Tests against a real ClickHouse (the one in `compose.yaml`). They are
//! ignored by default; CI and `cargo test -p dionaea-store -- --ignored` run
//! them. The server comes from `DIONAEA_TEST_CLICKHOUSE` as
//! `host:port:user:password`, defaulting to the compose file's.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use dionaea_store::{AliasRow, Config, DropRow, Error, EventRow, Store, TraitsRow};
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
            "drops_daily",
            "event_ids",
            "events",
            "identity_alias",
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
        "6"
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
    assert_eq!(count(&scratch.store, "_migrations"), 3);
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
        vec![("events", 3), ("identity_alias", 3), ("user_traits", 1)],
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
        vec![("events", 0), ("identity_alias", 0), ("user_traits", 0)],
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
        vec![("events", 0), ("identity_alias", 0), ("user_traits", 0)],
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
        vec![("events", 0), ("identity_alias", 0), ("user_traits", 0)]
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
        vec![("events", 0), ("identity_alias", 0), ("user_traits", 0)]
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
        vec![("events", 1), ("identity_alias", 2), ("user_traits", 0)],
        "the event of a2 and both links to it; no user, so no traits"
    );
    store.delete_user_rows(7, None, &device).expect("delete");
    let needed = store.rewrites_needed(7, None, &device).expect("needed");
    store.apply_rewrites(&needed).expect("rewrite");
    assert_eq!(
        rows(&store.count_user_rows(7, None, &device, true).expect("gone")),
        vec![("events", 0), ("identity_alias", 0), ("user_traits", 0)]
    );
    // Everything else stays: the other events, the other links, all traits.
    assert_eq!(count(store, "events"), 4);
    assert_eq!(count(store, "identity_alias"), 3);
    assert_eq!(count(store, "user_traits"), 3);
}
