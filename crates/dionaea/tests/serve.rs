//! The real binary as an operator runs it: create a site, a key and a schema
//! with the command line, start `serve`, send events over HTTP, and find them
//! durable in the spool. ClickHouse is not needed: the port used is closed, so
//! the events wait in the spool, which is exactly what must survive.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dionaea_spool::{Config as SpoolConfig, Spool};

const SCHEMA: &str =
    "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n";

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("serve")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn command(dir: &Path) -> Command {
    command_for(dir, "http://127.0.0.1:1", None)
}

/// A command whose ClickHouse is `url`, with the user and database of the
/// compose file or those given.
fn command_for(dir: &Path, url: &str, database: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    command
        .env("DIONAEA_METADATA_PATH", dir.join("meta.db"))
        .env("DIONAEA_SPOOL_DIR", dir.join("spool"))
        .env("DIONAEA_CLICKHOUSE_URL", url)
        .env("DIONAEA_CLICKHOUSE_USER", "dionaea")
        .env("DIONAEA_CLICKHOUSE_PASSWORD", "dionaea-dev")
        .env("DIONAEA_CLICKHOUSE_DATABASE", database.unwrap_or("dionaea"))
        .env("DIONAEA_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_OPS_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_LOG_FORMAT", "json");
    command
}

fn run(dir: &Path, args: &[&str]) -> String {
    let output = command(dir).args(args).output().expect("run the binary");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A running server, killed when dropped.
struct Server {
    child: Child,
    address: String,
    ops: String,
}

impl Server {
    fn start(dir: &Path) -> Self {
        Self::start_with(command(dir))
    }

    fn start_with(mut command: Command) -> Self {
        let mut child = command
            .arg("serve")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the server");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let (address, ops) = loop {
            let line = lines
                .next()
                .expect("the server logs before it exits")
                .expect("a line");
            if line.contains("\"message\":\"listening\"") {
                let value: serde_json::Value = serde_json::from_str(&line).expect("json log");
                let field = |name: &str| {
                    value["fields"][name]
                        .as_str()
                        .expect("an address")
                        .to_owned()
                };
                break (field("listen"), field("ops"));
            }
        };
        // Keep draining so the server never blocks writing its logs.
        std::thread::spawn(move || for _ in lines {});
        Self {
            child,
            address,
            ops,
        }
    }

    /// Sends one request and returns (status, headers and body as text).
    fn request(&self, head: &str, body: &str) -> (u16, String) {
        Self::request_to(&self.address, head, body)
    }

    /// A request to the operator listener.
    fn ops(&self, path: &str) -> (u16, String) {
        Self::request_to(&self.ops, &format!("GET {path} HTTP/1.1"), "")
    }

    fn request_to(address: &str, head: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let request = format!(
            "{head}\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("send");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status line");
        (status, response)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn batch(n: u32, items: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis();
    format!(
        "{{\"sent_at\":{now},\"sdk\":\"test/1.0\",\"events\":[{{\"event_id\":\"01890000-0000-7000-8000-{n:012}\",\"ts\":{now},\"name\":\"checkout\",\"anon_id\":\"a1\",\"props\":{{\"items\":{items}}}}}]}}"
    )
}

fn bootstrap(dir: &Path) -> String {
    run(dir, &["site", "create", "shop"]);
    let schema = dir.join("schema.toml");
    std::fs::write(&schema, SCHEMA).expect("write the schema");
    run(
        dir,
        &[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ],
    );
    run(dir, &["key", "issue", "--site", "1"])
}

#[test]
fn an_acknowledged_event_is_in_the_spool_after_the_server_is_killed() {
    let dir = dir("durable");
    let key = bootstrap(&dir);
    let server = Server::start(&dir);

    let post = |n: u32, items: u32| {
        server.request(
            &format!(
                "POST /v1/collect HTTP/1.1\r\nX-Dionaea-Key: {key}\r\nContent-Type: application/json"
            ),
            &batch(n, items),
        )
    };
    for n in 0..3 {
        let (status, response) = post(n, 2);
        assert_eq!(status, 202, "{response}");
        assert!(
            response.contains("{\"accepted\":1,\"dropped\":0}"),
            "{response}"
        );
    }
    let (status, _) = post(9, 1_000);
    assert_eq!(
        status, 202,
        "an invalid event still answers 202 and is counted as a drop"
    );

    // Wrong key, wrong method, wrong type.
    let (status, response) = server.request(
        "POST /v1/collect HTTP/1.1\r\nX-Dionaea-Key: wk_aaaaaaaaaaaaaaaaaaaaaaaaaa\r\nContent-Type: application/json",
        &batch(1, 1),
    );
    assert_eq!(status, 401, "{response}");
    let (status, _) = server.request("GET /v1/collect HTTP/1.1", "");
    assert_eq!(status, 405);
    assert_eq!(server.request("GET /healthz HTTP/1.1", "").0, 200);

    // Kill the process the hard way, then read what survived.
    drop(server);
    let mut config = SpoolConfig::new(dir.join("spool"));
    config.sync = false;
    let spool = Spool::open(config).expect("reopen the spool");
    let records = spool.read(100, usize::MAX).expect("read");
    assert_eq!(
        records.len(),
        3,
        "every request that kept an event is a durable record; the one that kept none only added to the drop counts held in memory"
    );
    let text: String = records
        .iter()
        .map(|r| String::from_utf8_lossy(&r.payload).into_owned())
        .collect();
    assert!(text.contains("01890000-0000-7000-8000-000000000001"));
}

#[test]
fn a_key_revoked_on_the_command_line_stops_working_without_a_restart() {
    let dir = dir("revoke");
    let key = bootstrap(&dir);
    let server = Server::start(&dir);
    let head =
        format!("POST /v1/collect?key={key} HTTP/1.1\r\nContent-Type: text/plain;charset=UTF-8");
    assert_eq!(server.request(&head, &batch(1, 1)).0, 202);

    run(&dir, &["key", "revoke", &key]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = server.request(&head, &batch(2, 1)).0;
        if status == 401 {
            break;
        }
        assert_eq!(status, 202);
        assert!(
            Instant::now() < deadline,
            "the revoked key still works after 15 s"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn server_with(dir: &Path, settings: &[(&str, &str)]) -> Server {
    let mut command = command(dir);
    for (name, value) in settings {
        command.env(format!("DIONAEA_{name}"), value);
    }
    Server::start_with(command)
}

fn collect_head(key: &str) -> String {
    format!("POST /v1/collect HTTP/1.1\r\nX-Dionaea-Key: {key}\r\nContent-Type: application/json")
}

fn metric(metrics: &str, line_start: &str) -> u64 {
    metrics
        .lines()
        .find(|line| line.starts_with(line_start))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no metric `{line_start}` in:\n{metrics}"))
}

#[test]
fn the_operator_endpoints_report_health_readiness_and_what_happened() {
    let dir = dir("ops");
    let key = bootstrap(&dir);
    let server = server_with(&dir, &[]);

    assert_eq!(server.ops("/healthz").0, 200);
    let (status, ready) = server.ops("/readyz");
    assert_eq!((status, ready.contains("ready")), (200, true), "{ready}");
    assert_eq!(
        server.request("GET /metrics HTTP/1.1", "").0,
        404,
        "the metrics are not on the public listener"
    );

    for n in 0..3 {
        assert_eq!(server.request(&collect_head(&key), &batch(n, 1)).0, 202);
    }
    server.request(&collect_head(&key), &batch(9, 1_000));
    server.request(
        "POST /v1/collect HTTP/1.1\r\nX-Dionaea-Key: wk_aaaaaaaaaaaaaaaaaaaaaaaaaa\r\nContent-Type: application/json",
        &batch(1, 1),
    );

    let (status, metrics) = server.ops("/metrics");
    assert_eq!(status, 200);
    assert_eq!(metric(&metrics, "dionaea_up "), 1);
    assert_eq!(
        metric(&metrics, "dionaea_requests_total{outcome=\"accepted\"}"),
        4
    );
    assert_eq!(
        metric(&metrics, "dionaea_requests_total{outcome=\"unknown_key\"}"),
        1
    );
    assert_eq!(
        metric(&metrics, "dionaea_events_total{result=\"accepted\"}"),
        3
    );
    assert_eq!(
        metric(&metrics, "dionaea_events_total{result=\"dropped\"}"),
        1
    );
    assert_eq!(
        metric(
            &metrics,
            "dionaea_events_dropped_total{reason=\"invalid_required_field\"}"
        ),
        1
    );
    assert!(metric(&metrics, "dionaea_spool_appended_records_total") >= 3);
    assert_eq!(metric(&metrics, "dionaea_spool_failed "), 0);
    assert_eq!(
        metric(&metrics, "dionaea_clickhouse_ready "),
        0,
        "ClickHouse is down here"
    );
    assert!(!metrics.contains(&key), "no key in the metrics");
}

#[test]
fn a_client_that_never_finishes_its_headers_is_disconnected() {
    let dir = dir("slow-headers");
    let server = server_with(&dir, &[("HEADER_TIMEOUT_SECS", "1")]);
    let mut stream = TcpStream::connect(&server.address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    stream
        .write_all(b"POST /v1/collect HTTP/1.1\r\nHost: x\r\nX-Slow: ")
        .expect("send");
    let started = Instant::now();
    let mut rest = Vec::new();
    // The server ends the connection; whatever it says first, it ends.
    let _ = stream.read_to_end(&mut rest);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(server.ops("/healthz").0, 200, "the server is fine");
}

#[test]
fn connections_beyond_the_limit_are_closed_and_counted() {
    let dir = dir("connections");
    let server = server_with(
        &dir,
        &[("MAX_CONNECTIONS", "2"), ("HEADER_TIMEOUT_SECS", "30")],
    );
    let hold = || {
        let stream = TcpStream::connect(&server.address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        stream
    };
    let (first, second) = (hold(), hold());
    // Wait until the server has seen both.
    let deadline = Instant::now() + Duration::from_secs(10);
    while metric(&server.ops("/metrics").1, "dionaea_connections_open") < 2 {
        assert!(
            Instant::now() < deadline,
            "the connections were not counted"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut third = hold();
    let mut byte = [0u8; 1];
    let closed = matches!(third.read(&mut byte), Ok(0) | Err(_));
    assert!(closed, "the third connection is closed at once");
    assert!(
        metric(
            &server.ops("/metrics").1,
            "dionaea_connections_total{result=\"refused\"}"
        ) >= 1
    );

    drop((first, second));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if server.request("GET /healthz HTTP/1.1", "").0 == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "no slot freed");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn behind_a_proxy_the_limit_follows_the_client_the_proxy_saw() {
    let dir = dir("proxy");
    let key = bootstrap(&dir);
    let server = server_with(
        &dir,
        &[
            ("TRUSTED_PROXY_HOPS", "1"),
            ("ADDRESS_BURST", "2"),
            ("ADDRESS_RATE_PER_SEC", "0.001"),
        ],
    );
    let from = |forwarded: &str| {
        server
            .request(
                &format!("{}\r\nX-Forwarded-For: {forwarded}", collect_head(&key)),
                &batch(1, 1),
            )
            .0
    };
    // What the client writes on the left is ignored: only the proxy's entry
    // on the right counts, so changing it does not escape the limit.
    assert_eq!(from("1.1.1.1, 9.9.9.9"), 202);
    assert_eq!(from("2.2.2.2, 9.9.9.9"), 202);
    assert_eq!(from("3.3.3.3, 9.9.9.9"), 429, "the same client again");
    assert_eq!(
        from("3.3.3.3, 8.8.8.8"),
        202,
        "another client is unaffected"
    );
}

#[test]
fn bots_and_retried_events_are_counted_and_not_stored_twice() {
    let dir = dir("filters");
    let key = bootstrap(&dir);
    let server = server_with(&dir, &[]);

    let bot = server.request(
        &format!("{}\r\nUser-Agent: Googlebot/2.1", collect_head(&key)),
        &batch(1, 1),
    );
    assert_eq!(bot.0, 202);
    assert!(
        bot.1.contains("{\"accepted\":0,\"dropped\":1}"),
        "{}",
        bot.1
    );

    let same = batch(7, 1);
    let first = server.request(&collect_head(&key), &same);
    assert!(
        first.1.contains("{\"accepted\":1,\"dropped\":0}"),
        "{}",
        first.1
    );
    let retry = server.request(&collect_head(&key), &same);
    assert!(
        retry.1.contains("{\"accepted\":0,\"dropped\":1}"),
        "{}",
        retry.1
    );

    let metrics = server.ops("/metrics").1;
    assert_eq!(
        metric(&metrics, "dionaea_events_dropped_total{reason=\"bot\"}"),
        1
    );
    assert_eq!(
        metric(
            &metrics,
            "dionaea_events_dropped_total{reason=\"duplicate_event\"}"
        ),
        1
    );
    assert_eq!(metric(&metrics, "dionaea_dedup_remembered_ids "), 1);
}

#[test]
fn requests_that_keep_no_event_do_not_cost_a_spool_record_each() {
    let dir = dir("drop-only");
    let key = bootstrap(&dir);
    let server = server_with(&dir, &[("DROP_FLUSH_SECS", "2")]);
    let records = |server: &Server| {
        metric(
            &server.ops("/metrics").1,
            "dionaea_spool_appended_records_total",
        )
    };
    let before = records(&server);
    for n in 0..30 {
        let (status, response) = server.request(
            &format!("{}\r\nUser-Agent: Googlebot/2.1", collect_head(&key)),
            &batch(n, 1),
        );
        assert_eq!(status, 202);
        assert!(
            response.contains("{\"accepted\":0,\"dropped\":1}"),
            "{response}"
        );
    }
    assert_eq!(
        records(&server),
        before,
        "thirty bot requests wrote nothing"
    );
    let metrics = server.ops("/metrics").1;
    assert_eq!(
        metric(&metrics, "dionaea_events_dropped_total{reason=\"bot\"}"),
        30
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while records(&server) == before {
        assert!(Instant::now() < deadline, "the counts were never written");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        records(&server) - before <= 2,
        "merged into one or two records"
    );
}

#[test]
fn a_retried_event_is_still_recognized_after_a_restart() {
    let dir = dir("dedup-restart");
    let key = bootstrap(&dir);
    let same = batch(7, 1);
    {
        let server = server_with(&dir, &[]);
        let first = server.request(&collect_head(&key), &same);
        assert!(
            first.1.contains("{\"accepted\":1,\"dropped\":0}"),
            "{}",
            first.1
        );
        // The journal is made durable every second.
        std::thread::sleep(Duration::from_millis(1800));
        drop(server);
    }
    let server = server_with(&dir, &[]);
    let retry = server.request(&collect_head(&key), &same);
    assert!(
        retry.1.contains("{\"accepted\":0,\"dropped\":1}"),
        "{}",
        retry.1
    );
    assert_eq!(
        metric(
            &server.ops("/metrics").1,
            "dionaea_events_dropped_total{reason=\"duplicate_event\"}"
        ),
        1
    );
}

/// Against the ClickHouse of `compose.yaml` (`docker compose up -d`): the
/// whole product on one machine, including a hard kill between the
/// acknowledgement and the insert.
#[test]
#[ignore = "needs ClickHouse"]
fn killed_after_acknowledging_the_events_still_reach_clickhouse_once_restarted() {
    use dionaea_store::{Config, Store};

    let database = format!("dionaea_serve_{}", std::process::id());
    let make = |database: &str| {
        let mut config = Config::new("127.0.0.1", 8123, database);
        config.user = "dionaea".to_owned();
        config.password = "dionaea-dev".to_owned();
        Store::new(config).expect("a valid config")
    };
    let admin = make("default");
    admin
        .raw_query(&format!("CREATE DATABASE {database}"))
        .expect("create the database");
    struct Cleanup(Store, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.1));
        }
    }
    let _cleanup = Cleanup(admin, database.clone());

    let dir = dir("clickhouse");
    let key = {
        let run_here = |args: &[&str]| {
            let output = command_for(&dir, "http://127.0.0.1:8123", Some(&database))
                .args(args)
                .output()
                .expect("run the binary");
            assert!(output.status.success(), "{args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_here(&["site", "create", "shop"]);
        let schema = dir.join("schema.toml");
        std::fs::write(&schema, SCHEMA).expect("write the schema");
        run_here(&[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ]);
        run_here(&["key", "issue", "--site", "1"])
    };

    let first = Server::start_with(command_for(&dir, "http://127.0.0.1:8123", Some(&database)));
    let head = format!(
        "POST /v1/collect HTTP/1.1\r\nX-Dionaea-Key: {key}\r\nContent-Type: application/json"
    );
    for n in 0..30 {
        assert_eq!(first.request(&head, &batch(n, 1)).0, 202);
    }
    // No orderly shutdown: the process is killed with whatever it had.
    drop(first);

    let second = Server::start_with(command_for(&dir, "http://127.0.0.1:8123", Some(&database)));
    let store = make(&database);
    let number = |sql: &str| -> u64 {
        store
            .raw_query(sql)
            .map(|s| s.trim().parse().unwrap_or(0))
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while number("SELECT uniqExact(event_id) FROM events") < 30 {
        assert!(Instant::now() < deadline, "the events did not arrive");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        number("SELECT uniqExact(event_id) FROM events"),
        30,
        "none lost"
    );
    assert!(
        number("SELECT count() FROM events") <= 60,
        "duplicates stay within one batch per restart"
    );
    drop(second);
}

/// The server talks to ClickHouse over HTTPS (`compose.tls.yaml`, certificates
/// from `scripts/gen-dev-certs.sh`), verifying the certificate.
#[test]
#[ignore = "needs ClickHouse with HTTPS"]
fn events_reach_clickhouse_over_tls() {
    use dionaea_store::{Config, Store};

    let ca = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.dev-certs/ca.pem");
    let database = format!("dionaea_serve_tls_{}", std::process::id());
    let make = |database: &str| {
        let mut config = Config::new("127.0.0.1", 8123, database);
        config.user = "dionaea".to_owned();
        config.password = "dionaea-dev".to_owned();
        Store::new(config).expect("a valid config")
    };
    let admin = make("default");
    admin
        .raw_query(&format!("CREATE DATABASE {database}"))
        .expect("create the database");
    struct Cleanup(Store, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self
                .0
                .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.1));
        }
    }
    let _cleanup = Cleanup(admin, database.clone());

    let dir = dir("tls");
    let key = bootstrap(&dir);
    let tls = |mut command: Command| {
        command
            .env("DIONAEA_CLICKHOUSE_URL", "https://127.0.0.1:8443")
            .env("DIONAEA_CLICKHOUSE_CA_FILE", &ca)
            .env("DIONAEA_CLICKHOUSE_SERVER_NAME", "localhost")
            .env("DIONAEA_CLICKHOUSE_DATABASE", &database);
        command
    };
    let server = Server::start_with(tls(command(&dir)));
    for n in 0..5 {
        assert_eq!(server.request(&collect_head(&key), &batch(n, 1)).0, 202);
    }
    let store = make(&database);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let rows = store
            .raw_query("SELECT uniqExact(event_id) FROM events")
            .map_or(0, |text| text.trim().parse().unwrap_or(0));
        if rows == 5 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the events did not arrive over TLS"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        metric(&server.ops("/metrics").1, "dionaea_clickhouse_ready "),
        1
    );
    drop(server);

    // A server that is not told to trust that CA never prepares ClickHouse and
    // keeps what it was sent in the spool instead of losing it.
    let mut command = command(&dir);
    command
        .env("DIONAEA_CLICKHOUSE_URL", "https://127.0.0.1:8443")
        .env("DIONAEA_CLICKHOUSE_SERVER_NAME", "localhost")
        .env("DIONAEA_CLICKHOUSE_DATABASE", &database);
    let untrusting = Server::start_with(command);
    assert_eq!(
        untrusting.request(&collect_head(&key), &batch(50, 1)).0,
        202
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        metric(&untrusting.ops("/metrics").1, "dionaea_clickhouse_ready "),
        0
    );
}

/// A database of its own on the ClickHouse of `compose.yaml`, dropped when the
/// guard goes out of scope.
struct ScratchDatabase {
    admin: dionaea_store::Store,
    name: String,
}

impl ScratchDatabase {
    fn new(tag: &str) -> Self {
        let name = format!("dionaea_serve_{tag}_{}", std::process::id());
        let admin = Self::store("default");
        admin
            .raw_query(&format!("CREATE DATABASE {name}"))
            .expect("create the database");
        Self { admin, name }
    }

    fn store(database: &str) -> dionaea_store::Store {
        let mut config = dionaea_store::Config::new("127.0.0.1", 8123, database);
        config.user = "dionaea".to_owned();
        config.password = "dionaea-dev".to_owned();
        dionaea_store::Store::new(config).expect("a valid config")
    }

    fn number(&self, sql: &str) -> u64 {
        Self::store(&self.name)
            .raw_query(sql)
            .map_or(0, |text| text.trim().parse().unwrap_or(0))
    }

    fn text(&self, sql: &str) -> String {
        Self::store(&self.name)
            .raw_query(sql)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default()
    }

    fn wait_for(&self, what: &str, sql: &str, expected: u64) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.number(sql) != expected {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for ScratchDatabase {
    fn drop(&mut self) {
        let _ = self
            .admin
            .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.name));
    }
}

fn in_clickhouse(dir: &Path, database: &str) -> Command {
    command_for(dir, "http://127.0.0.1:8123", Some(database))
}

/// Two servers with their own spools and one ClickHouse, as two replicas
/// behind a balancer: a batch the SDK sent to one and then sent again to the
/// other is stored once.
#[test]
#[ignore = "needs ClickHouse"]
fn a_retry_that_lands_on_another_replica_is_stored_once() {
    let database = ScratchDatabase::new("replicas");
    let dir = dir("replicas");
    let key = {
        let run_here = |args: &[&str]| {
            let output = in_clickhouse(&dir, &database.name)
                .args(args)
                .output()
                .expect("run the binary");
            assert!(output.status.success(), "{args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_here(&["site", "create", "shop"]);
        let schema = dir.join("schema.toml");
        std::fs::write(&schema, SCHEMA).expect("write the schema");
        run_here(&[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ]);
        run_here(&["key", "issue", "--site", "1"])
    };

    // The second replica shares the metadata and has a spool and an event id
    // journal of its own.
    let mut second_command = in_clickhouse(&dir, &database.name);
    second_command.env("DIONAEA_SPOOL_DIR", dir.join("spool-b"));
    let first = Server::start_with(in_clickhouse(&dir, &database.name));
    let second = Server::start_with(second_command);

    let events: Vec<String> = (0..3).map(|n| batch(n, 1)).collect();
    for body in &events {
        assert_eq!(first.request(&collect_head(&key), body).0, 202);
    }
    database.wait_for(
        "the first replica",
        "SELECT uniqExact(event_id) FROM events",
        3,
    );
    database.wait_for("their ids", "SELECT count() FROM event_ids", 3);

    // The retries reach the other replica, which has never seen them.
    for body in &events {
        let answer = second.request(&collect_head(&key), body);
        assert!(
            answer.1.contains("{\"accepted\":1,\"dropped\":0}"),
            "{}",
            answer.1
        );
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while metric(
        &second.ops("/metrics").1,
        "dionaea_batcher_duplicates_total",
    ) < 3
    {
        assert!(
            Instant::now() < deadline,
            "the second replica never left them out"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(
        database.number("SELECT count() FROM events"),
        3,
        "each event once"
    );
    assert_eq!(database.number("SELECT uniqExact(event_id) FROM events"), 3);
}

/// The enrichment end to end with MaxMind's own test database in
/// `DIONAEA_TEST_GEOIP`: the country of the forwarded client address and the
/// browser from the user agent are stored; the address is not.
#[test]
#[ignore = "needs ClickHouse and a MaxMind database in DIONAEA_TEST_GEOIP"]
fn the_country_and_browser_of_a_client_are_stored_and_its_address_is_not() {
    let geoip = std::env::var("DIONAEA_TEST_GEOIP").expect("DIONAEA_TEST_GEOIP names the file");
    let database = ScratchDatabase::new("enrich");
    let dir = dir("enrich");
    let key = {
        let run_here = |args: &[&str]| {
            let output = in_clickhouse(&dir, &database.name)
                .args(args)
                .output()
                .expect("run the binary");
            assert!(output.status.success(), "{args:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run_here(&["site", "create", "shop"]);
        let schema = dir.join("schema.toml");
        std::fs::write(&schema, SCHEMA).expect("write the schema");
        run_here(&[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ]);
        run_here(&["key", "issue", "--site", "1"])
    };
    let mut command = in_clickhouse(&dir, &database.name);
    command
        .env("DIONAEA_GEOIP_PATH", &geoip)
        .env("DIONAEA_TRUSTED_PROXY_HOPS", "1");
    let server = Server::start_with(command);

    let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0";
    let head = format!(
        "{}\r\nX-Forwarded-For: 216.160.83.56\r\nUser-Agent: {firefox}",
        collect_head(&key)
    );
    assert_eq!(server.request(&head, &batch(1, 1)).0, 202);
    database.wait_for("the event", "SELECT count() FROM events", 1);
    assert_eq!(
        database.text("SELECT concat(country, '|', platform, '|', os, '|', browser, '|', device_class) FROM events"),
        "US|web|linux|firefox|desktop"
    );
    let everything = database.text("SELECT toJSONString(tuple(*)) FROM events");
    assert!(
        !everything.contains("216.160.83.56"),
        "the address is not stored: {everything}"
    );
}
