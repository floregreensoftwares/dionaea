//! The real binary as an operator runs it: create a site, a key and a schema
//! with the command line, start `serve`, send events over HTTP, and find them
//! durable in the spool. ClickHouse is not needed: the port used is closed, so
//! the events wait in the spool, which is exactly what must survive.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dionaea_spool::{Config as SpoolConfig, Spool};

const SCHEMA: &str =
    "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n";

/// The events schema plus two identity traits.
const SCHEMA_WITH_TRAITS: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n\n[traits.plan]\nkind = \"id\"\n\n[traits.seats]\nkind = \"count\"\nmax = 1000\n";

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
    logs: Arc<Mutex<Vec<String>>>,
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
        let mut early = Vec::new();
        let (address, ops) = loop {
            let line = lines
                .next()
                .expect("the server logs before it exits")
                .expect("a line");
            early.push(line.clone());
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
        // Keep draining so the server never blocks writing its logs, and keep
        // what it wrote for tests that read it.
        let logs = Arc::new(Mutex::new(early));
        let kept = Arc::clone(&logs);
        std::thread::spawn(move || {
            for line in lines.map_while(Result::ok) {
                kept.lock().expect("logs").push(line);
            }
        });
        Self {
            child,
            address,
            ops,
            logs,
        }
    }

    /// Every log line written so far.
    fn logs(&self) -> Vec<String> {
        self.logs.lock().expect("logs").clone()
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
        // A server that refuses the connection (at its limit) may close it
        // before the request is written or reset it while it is read; that is
        // answered with status 0 rather than a panic, so a caller can retry.
        let Ok(mut stream) = TcpStream::connect(address) else {
            return (0, String::new());
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let request = format!(
            "{head}\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return (0, String::new());
        }
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
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
    bootstrap_with(dir, SCHEMA)
}

fn bootstrap_with(dir: &Path, schema_text: &str) -> String {
    run(dir, &["site", "create", "shop"]);
    let schema = dir.join("schema.toml");
    std::fs::write(&schema, schema_text).expect("write the schema");
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

// Settings stored with the command line reach a running server.

/// Runs the binary and returns (exit code, stdout, stderr).
fn run_status(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = command(dir).args(args).output().expect("run the binary");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    )
}

/// Polls `condition` for up to `seconds`.
fn eventually(what: &str, seconds: u64, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_limit_stored_on_the_command_line_takes_hold_in_a_running_server_and_lifts_when_removed() {
    let dir = dir("hot-limit");
    let key = bootstrap(&dir);
    let server = server_with(&dir, &[("REFRESH_SECS", "1")]);
    let post = || server.request(&collect_head(&key), &batch(1, 1)).0;
    for n in 0..5 {
        assert_eq!(
            server.request(&collect_head(&key), &batch(n + 100, 1)).0,
            202
        );
    }

    // No restart: a burst of two and almost no refill.
    assert_eq!(
        run_status(&dir, &["settings", "set", "address_burst", "2"]).0,
        0
    );
    assert_eq!(
        run_status(&dir, &["settings", "set", "address_rate_per_sec", "0.001"]).0,
        0
    );
    eventually("the limit to take hold", 15, || post() == 429);
    let metrics = server.ops("/metrics").1;
    assert_eq!(metric(&metrics, "dionaea_settings_stored "), 2);
    assert_eq!(metric(&metrics, "dionaea_settings_skipped "), 0);

    // Removed, the default applies again.
    assert_eq!(
        run_status(&dir, &["settings", "unset", "address_burst"]).0,
        0
    );
    assert_eq!(
        run_status(&dir, &["settings", "unset", "address_rate_per_sec"]).0,
        0
    );
    eventually("the limit to lift", 15, || post() == 202);
    assert_eq!(
        metric(&server.ops("/metrics").1, "dionaea_settings_stored "),
        0
    );
}

#[test]
fn a_connection_limit_stored_while_the_server_runs_applies_to_new_connections() {
    let dir = dir("hot-connections");
    let server = server_with(
        &dir,
        &[("REFRESH_SECS", "1"), ("HEADER_TIMEOUT_SECS", "30")],
    );
    assert_eq!(
        run_status(&dir, &["settings", "set", "max_connections", "1"]).0,
        0
    );
    eventually("one connection to be the limit", 15, || {
        let first = TcpStream::connect(&server.address).expect("connect");
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        std::thread::sleep(Duration::from_millis(150));
        let mut second = TcpStream::connect(&server.address).expect("connect");
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let mut byte = [0u8; 1];
        // Closed by the server: end of stream, or a reset. A read that merely
        // times out means the server kept the connection.
        let refused = match second.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        drop((first, second));
        std::thread::sleep(Duration::from_millis(150));
        refused
    });
    assert_eq!(
        run_status(&dir, &["settings", "unset", "max_connections"]).0,
        0
    );
    eventually("the limit to lift", 15, || {
        let first = TcpStream::connect(&server.address).expect("connect");
        let mut second = TcpStream::connect(&server.address).expect("connect");
        second
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .expect("send");
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let mut answer = String::new();
        let _ = second.read_to_string(&mut answer);
        drop(first);
        answer.contains("200")
    });
}

#[test]
fn settings_stored_before_the_server_starts_apply_from_the_first_request() {
    let dir = dir("stored-first");
    let key = bootstrap(&dir);
    run_status(&dir, &["settings", "set", "address_burst", "1", "--now"]);
    run_status(
        &dir,
        &["settings", "set", "address_rate_per_sec", "0.001", "--now"],
    );
    let server = server_with(&dir, &[]);
    let head = collect_head(&key);
    assert_eq!(server.request(&head, &batch(1, 1)).0, 202);
    assert_eq!(server.request(&head, &batch(2, 1)).0, 429, "a burst of one");
}

#[test]
fn a_stored_value_that_is_out_of_range_is_ignored_and_reported_not_fatal() {
    let dir = dir("stored-bad");
    let key = bootstrap(&dir);
    // Written behind the command line's back, as a hand edit of the file could.
    let meta = dionaea_meta::SqliteMeta::open(&dir.join("meta.db")).expect("open");
    dionaea_meta::MetaStore::set_setting(&meta, "key_burst", Some("0"), 0, 0).expect("store");
    dionaea_meta::MetaStore::set_setting(&meta, "no_such_setting", Some("1"), 0, 0).expect("store");
    dionaea_meta::MetaStore::set_setting(&meta, "address_burst", Some("3"), 0, 0).expect("store");
    drop(meta);

    let server = server_with(&dir, &[("ADDRESS_RATE_PER_SEC", "0.001")]);
    let head = collect_head(&key);
    for n in 0..3 {
        assert_eq!(
            server.request(&head, &batch(n, 1)).0,
            202,
            "the good value applies"
        );
    }
    assert_eq!(
        server.request(&head, &batch(9, 1)).0,
        429,
        "a burst of three"
    );
    let metrics = server.ops("/metrics").1;
    assert_eq!(metric(&metrics, "dionaea_settings_stored "), 3);
    assert_eq!(metric(&metrics, "dionaea_settings_skipped "), 2);
    assert_eq!(server.ops("/readyz").0, 200);
}

#[test]
fn the_command_line_validates_lists_and_removes_settings() {
    let dir = dir("settings-cli");
    bootstrap(&dir);

    let (code, _, err) = run_status(&dir, &["settings", "set", "key_burst", "0.1"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("key_burst") && err.contains("from 1 to 1e9"),
        "{err}"
    );
    let (code, _, err) = run_status(&dir, &["settings", "set", "listen", "127.0.0.1:1"]);
    assert_eq!(code, 1);
    assert!(err.contains("key_burst"), "it names what can change: {err}");
    assert_eq!(run_status(&dir, &["settings", "get", "nope"]).0, 1);
    assert_eq!(run_status(&dir, &["settings", "unset", "nope"]).0, 1);

    assert_eq!(
        run_status(&dir, &["settings", "set", "key_burst", "0900", "--now"]).0,
        0
    );
    let (code, value, _) = run_status(&dir, &["settings", "get", "key_burst"]);
    assert_eq!(
        (code, value.as_str()),
        (0, "900"),
        "stored as 900, not as written"
    );
    let (_, list, _) = run_status(&dir, &["settings", "list"]);
    let line = list
        .lines()
        .find(|l| l.starts_with("key_burst\t"))
        .expect("listed");
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(&fields[..3], ["key_burst", "900", "stored"]);
    let other = list
        .lines()
        .find(|l| l.starts_with("address_burst\t"))
        .expect("listed");
    assert!(
        other.starts_with("address_burst\t100\tdefault\t"),
        "{other}"
    );

    let (code, _, err) = run_status(&dir, &["settings", "unset", "key_burst", "--now"]);
    assert_eq!((code, err.as_str()), (0, ""));
    let (_, again, _) = run_status(&dir, &["settings", "unset", "key_burst", "--now"]);
    assert_eq!(again, "");
    let (_, value, _) = run_status(&dir, &["settings", "get", "key_burst"]);
    assert_eq!(value, "400");
}

#[test]
fn the_file_and_the_environment_show_as_the_source_of_a_setting() {
    let dir = dir("settings-sources");
    bootstrap(&dir);
    let config = dir.join("dionaea.toml");
    std::fs::write(&config, "key_burst = 555\naddress_burst = 66\n").expect("write");
    let list = |extra: &[(&str, &str)]| {
        let mut command = command(&dir);
        command.args(["settings", "list", "--config"]).arg(&config);
        for (name, value) in extra {
            command.env(name, value);
        }
        String::from_utf8_lossy(&command.output().expect("run").stdout).into_owned()
    };
    let text = list(&[("DIONAEA_ADDRESS_BURST", "77")]);
    assert!(text.contains("key_burst\t555\tfile\t"), "{text}");
    assert!(text.contains("address_burst\t77\tenvironment\t"), "{text}");
    run_status(&dir, &["settings", "set", "key_burst", "10", "--now"]);
    let text = list(&[]);
    assert!(
        text.contains("key_burst\t10\tstored (overrides file: 555)\t"),
        "stored wins over the file, and the list says what it hides: {text}"
    );
}

// Changes that take effect at a stated moment.

#[test]
fn two_servers_on_one_store_switch_to_a_stored_limit_at_the_same_moment() {
    let dir = dir("replicas-switch");
    let key = bootstrap(&dir);
    let spool_b = dir.join("spool-b");
    let spool_b = spool_b.to_str().expect("utf-8");
    let first = server_with(&dir, &[("REFRESH_SECS", "1")]);
    let second = server_with(&dir, &[("REFRESH_SECS", "1"), ("SPOOL_DIR", spool_b)]);

    let started = Instant::now();
    assert_eq!(
        run_status(
            &dir,
            &["settings", "set", "address_burst", "1", "--after", "3"]
        )
        .0,
        0
    );
    assert_eq!(
        run_status(
            &dir,
            &[
                "settings",
                "set",
                "address_rate_per_sec",
                "0.001",
                "--after",
                "3"
            ]
        )
        .0,
        0
    );

    // Each server is probed about twenty times a second until it refuses.
    let mut refused_at: [Option<Duration>; 2] = [None, None];
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut n = 0;
    while refused_at.iter().any(Option::is_none) {
        assert!(
            Instant::now() < deadline,
            "a server never switched: {refused_at:?}"
        );
        for (index, server) in [&first, &second].into_iter().enumerate() {
            n += 1;
            let status = server.request(&collect_head(&key), &batch(n, 1)).0;
            if status == 429 && refused_at[index].is_none() {
                refused_at[index] = Some(started.elapsed());
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let [a, b] = refused_at.map(|t| t.expect("both switched"));
    assert!(
        a >= Duration::from_millis(2_000),
        "not before its time: {a:?}"
    );
    assert!(
        b >= Duration::from_millis(2_000),
        "not before its time: {b:?}"
    );
    let gap = a.abs_diff(b);
    assert!(
        gap < Duration::from_millis(1_000),
        "the servers switched {gap:?} apart ({a:?} and {b:?})"
    );
}

#[test]
fn a_change_waits_for_its_moment_and_the_list_says_so() {
    let dir = dir("pending");
    bootstrap(&dir);
    let value = || run_status(&dir, &["settings", "get", "key_burst"]).1;
    assert_eq!(value(), "400");
    assert_eq!(
        run_status(
            &dir,
            &["settings", "set", "key_burst", "900", "--after", "4"]
        )
        .0,
        0
    );
    assert_eq!(value(), "400", "the old value stays until then");
    let (_, list, _) = run_status(&dir, &["settings", "list"]);
    let line = list
        .lines()
        .find(|l| l.starts_with("key_burst\t"))
        .expect("listed");
    assert!(
        line.contains("; 900 in "),
        "the list shows what is coming: {line}"
    );

    eventually("the change to take effect", 15, || value() == "900");
    let (_, list, _) = run_status(&dir, &["settings", "list"]);
    assert!(
        list.lines()
            .any(|l| l.starts_with("key_burst\t900\tstored\t")),
        "{list}"
    );
}

#[test]
fn a_removal_is_scheduled_like_a_value() {
    let dir = dir("pending-removal");
    bootstrap(&dir);
    run_status(&dir, &["settings", "set", "key_burst", "900", "--now"]);
    assert_eq!(
        run_status(&dir, &["settings", "unset", "key_burst", "--after", "3"]).0,
        0
    );
    let (_, list, _) = run_status(&dir, &["settings", "list"]);
    let line = list
        .lines()
        .find(|l| l.starts_with("key_burst\t"))
        .expect("listed");
    assert!(line.starts_with("key_burst\t900\tstored"), "{line}");
    assert!(line.contains("; removed in "), "{line}");
    eventually("the removal to take effect", 15, || {
        run_status(&dir, &["settings", "get", "key_burst"]).1 == "400"
    });
}

#[test]
fn now_and_after_exclude_each_other() {
    let dir = dir("when-args");
    bootstrap(&dir);
    let (code, _, err) = run_status(
        &dir,
        &["settings", "set", "key_burst", "5", "--now", "--after", "3"],
    );
    assert_ne!(code, 0);
    assert!(err.contains("cannot be used with"), "{err}");
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

#[test]
fn the_logs_never_hold_a_key_an_address_or_what_a_client_sent() {
    let dir = dir("logs");
    let key = bootstrap(&dir);
    let server = server_with(&dir, &[("LOG_LEVEL", "trace"), ("TRUSTED_PROXY_HOPS", "1")]);
    let from = |forwarded: &str, agent: &str, body: &str, key: &str| {
        server.request(
            &format!(
                "{}\r\nX-Forwarded-For: {forwarded}\r\nUser-Agent: {agent}\r\nOrigin: https://marker-origin.example\r\nReferer: https://marker-referer.example/path",
                collect_head(key)
            ),
            body,
        )
    };
    let hostile = |name: &str, prop: &str| {
        format!(
            "{{\"sent_at\":1,\"sdk\":\"marker-sdk\",\"events\":[{{\"event_id\":\"01890000-0000-7000-8000-000000000001\",\"ts\":1,\"name\":\"{name}\",\"anon_id\":\"marker-anon\",\"props\":{{\"items\":1,\"free\":\"{prop}\"}}}}]}}"
        )
    };

    from(
        "203.0.113.77",
        "MarkerAgent/9",
        &hostile("checkout", "marker-free-text"),
        &key,
    );
    from(
        "2001:db8:1234:5678::77",
        "MarkerAgent/9",
        &hostile("marker-undeclared-event", "marker-free-text"),
        &key,
    );
    from("203.0.113.77", "Googlebot/2.1", &batch(2, 1), &key);
    from("203.0.113.77", "MarkerAgent/9", "{marker-not-json", &key);
    from(
        "203.0.113.77",
        "MarkerAgent/9",
        &batch(3, 1),
        "wk_markerwrongkey0000000000",
    );
    // Identify carries a user id and traits, which are just as private.
    let identify = r#"{"sent_at":1,"sdk":"marker-sdk","anon_id":"marker-anon","user_id":"marker-user","traits":{"marker-trait":"marker-value"}}"#;
    server.request(
        &format!(
            "POST /v1/identify HTTP/1.1
X-Dionaea-Key: {key}
Content-Type: application/json
X-Forwarded-For: 203.0.113.77
User-Agent: MarkerAgent/9"
        ),
        identify,
    );
    server.request(
        &format!(
            "POST /v1/identify HTTP/1.1
X-Dionaea-Key: {key}
Content-Type: application/json
X-Forwarded-For: 203.0.113.77
User-Agent: MarkerAgent/9"
        ),
        "{marker-not-json",
    );
    server.ops("/metrics");

    // The capture keeps working after start: ClickHouse is down here, and the
    // server says so.
    eventually("a log line after start", 20, || {
        server
            .logs()
            .iter()
            .any(|line| line.contains("clickhouse is not ready"))
    });
    let logs = server.logs();
    for forbidden in [
        key.as_str(),
        "wk_markerwrongkey",
        "203.0.113.77",
        "2001:db8",
        "marker-",
        "MarkerAgent",
        "Googlebot",
    ] {
        let hits: Vec<_> = logs
            .iter()
            .filter(|line| line.contains(forbidden))
            .collect();
        assert!(hits.is_empty(), "`{forbidden}` is in the logs: {hits:?}");
    }
}

/// The most memory the server may hold at its peak under this test's load.
const MEMORY_BOUND_BYTES: u64 = 256 * 1024 * 1024;

/// The peak resident memory of a process.
fn peak_resident_bytes(pid: u32) -> u64 {
    if cfg!(windows) {
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).PeakWorkingSet64"),
            ])
            .output()
            .expect("run powershell");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("a byte count")
    } else {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("proc status");
        let kib: u64 = status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("VmHWM");
        kib * 1024
    }
}

fn big_batch(n: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis();
    let events: Vec<String> = (0..100)
        .map(|i| {
            format!(
                "{{\"event_id\":\"01890000-0000-7000-8000-{:012}\",\"ts\":{now},\"name\":\"checkout\",\"anon_id\":\"a{i}\",\"props\":{{\"items\":{i}}}}}",
                u64::from(n) * 100 + i
            )
        })
        .collect();
    format!(
        "{{\"sent_at\":{now},\"sdk\":\"test/1.0\",\"events\":[{}]}}",
        events.join(",")
    )
}

#[test]
#[ignore = "sustained load for about 20 seconds"]
fn the_server_stays_inside_its_memory_bound_under_sustained_load_with_clickhouse_down() {
    let dir = dir("load");
    let key = bootstrap(&dir);
    let server = server_with(
        &dir,
        &[
            ("KEY_RATE_PER_SEC", "100000"),
            ("KEY_BURST", "100000"),
            ("ADDRESS_RATE_PER_SEC", "100000"),
            ("ADDRESS_BURST", "100000"),
            ("MAX_CONNECTIONS", "64"),
            ("SPOOL_QUOTA_BYTES", "33554432"),
        ],
    );
    let stop = Instant::now() + Duration::from_secs(20);
    let counts: Vec<std::collections::BTreeMap<u16, u64>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..48u32)
            .map(|worker| {
                let (server, key) = (&server, &key);
                scope.spawn(move || {
                    let mut counts = std::collections::BTreeMap::new();
                    let mut n = worker * 1_000_000;
                    while Instant::now() < stop {
                        n += 1;
                        let (status, _) = server.request(&collect_head(key), &big_batch(n));
                        *counts.entry(status).or_insert(0u64) += 1;
                    }
                    counts
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("worker"))
            .collect()
    });
    let mut total = std::collections::BTreeMap::<u16, u64>::new();
    for (status, n) in counts.iter().flatten() {
        *total.entry(*status).or_insert(0) += n;
    }
    eprintln!("answers under load: {total:?}");

    assert!(total.get(&202).copied().unwrap_or(0) > 100, "{total:?}");
    assert!(
        total.get(&503).copied().unwrap_or(0) > 0,
        "the quota was reached, so the full spool was exercised: {total:?}"
    );
    for status in total.keys() {
        assert!(
            [0, 202, 503].contains(status),
            "only acknowledgements, refusals and closed connections: {total:?}"
        );
    }
    assert_eq!(server.ops("/healthz").0, 200, "alive after the load");
    let (_, metrics) = server.ops("/metrics");
    let spooled = metric(&metrics, "dionaea_spool_bytes ");
    assert!(spooled <= 33_554_432 + 4 * 1024 * 1024, "{spooled}");

    let peak = peak_resident_bytes(server.child.id());
    eprintln!("peak resident memory: {} MiB", peak / (1024 * 1024));
    assert!(
        peak < MEMORY_BOUND_BYTES,
        "peak {} MiB exceeds the bound of {} MiB",
        peak / (1024 * 1024),
        MEMORY_BOUND_BYTES / (1024 * 1024)
    );
}

/// A TCP relay to ClickHouse that can be cut and restored, so an outage is
/// the same on every platform: new connections are closed at once and the
/// open ones are shut down.
struct Relay {
    address: String,
    up: Arc<std::sync::atomic::AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
}

impl Relay {
    fn start(upstream: &'static str) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the relay");
        let address = listener.local_addr().expect("address").to_string();
        let up = Arc::new(AtomicBool::new(true));
        let open = Arc::new(Mutex::new(Vec::<TcpStream>::new()));
        let (flag, streams) = (Arc::clone(&up), Arc::clone(&open));
        std::thread::spawn(move || {
            for client in listener.incoming().map_while(Result::ok) {
                if !flag.load(Ordering::SeqCst) {
                    continue;
                }
                let Ok(server) = TcpStream::connect(upstream) else {
                    continue;
                };
                {
                    let mut open = streams.lock().expect("streams");
                    open.push(client.try_clone().expect("clone"));
                    open.push(server.try_clone().expect("clone"));
                }
                for (mut from, mut to) in [
                    (
                        client.try_clone().expect("clone"),
                        server.try_clone().expect("clone"),
                    ),
                    (server, client),
                ] {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(std::net::Shutdown::Both);
                    });
                }
            }
        });
        Self { address, up, open }
    }

    fn cut(&self) {
        self.up.store(false, std::sync::atomic::Ordering::SeqCst);
        for stream in self.open.lock().expect("streams").drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn restore(&self) {
        self.up.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// ClickHouse becomes unreachable while the server runs: every request is
/// still acknowledged and stays in the spool, the server stays ready, and when
/// ClickHouse is back each acknowledged event is stored once.
#[test]
#[ignore = "needs ClickHouse"]
fn an_outage_of_clickhouse_loses_nothing_and_never_stops_the_server_answering() {
    let database = ScratchDatabase::new("outage");
    let dir = dir("outage");
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
    let relay = Relay::start("127.0.0.1:8123");
    let server = Server::start_with(command_for(
        &dir,
        &format!("http://{}", relay.address),
        Some(&database.name),
    ));
    eventually("ClickHouse prepared", 60, || {
        metric(&server.ops("/metrics").1, "dionaea_clickhouse_ready ") == 1
    });

    let send = |from: u32, to: u32| {
        for n in from..to {
            assert_eq!(server.request(&collect_head(&key), &batch(n, 1)).0, 202);
        }
    };
    send(0, 20);
    database.wait_for(
        "the first events",
        "SELECT uniqExact(event_id) FROM events",
        20,
    );

    relay.cut();
    send(20, 80);
    eventually("the batcher noticing the outage", 60, || {
        metric(&server.ops("/metrics").1, "dionaea_batcher_failing ") == 1
    });
    assert_eq!(server.ops("/readyz").0, 200, "an outage is not unreadiness");
    assert_eq!(server.ops("/healthz").0, 200);
    send(80, 90);
    assert_eq!(
        database.number("SELECT uniqExact(event_id) FROM events"),
        20,
        "nothing reached ClickHouse while it was cut"
    );

    relay.restore();
    database.wait_for("every event", "SELECT uniqExact(event_id) FROM events", 90);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        database.number("SELECT count() FROM events"),
        90,
        "each acknowledged event stored once"
    );
    eventually("the batcher recovering", 30, || {
        metric(&server.ops("/metrics").1, "dionaea_batcher_failing ") == 0
    });
}

/// The server is killed again and again at different moments while a client
/// keeps sending: during appends, during the wait for the sync, between the
/// insert and the commit. Every event that was acknowledged is in ClickHouse
/// at the end, and duplicates stay within the documented bound.
#[test]
#[ignore = "needs ClickHouse"]
fn repeated_kills_while_sending_lose_no_acknowledged_event() {
    use std::sync::atomic::{AtomicBool, Ordering};

    const KILLS: u32 = 6;
    let database = ScratchDatabase::new("kills");
    let dir = dir("kills");
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

    let mut acknowledged = std::collections::BTreeSet::new();
    let mut next = 0u32;
    for round in 0..KILLS {
        let server = Server::start_with(in_clickhouse(&dir, &database.name));
        let stop = AtomicBool::new(false);
        let address = server.address.clone();
        let (last, acked) = std::thread::scope(|scope| {
            let (stop, key, address) = (&stop, &key, &address);
            let sender = scope.spawn(move || {
                let mut acked = Vec::new();
                let mut n = next;
                while !stop.load(Ordering::SeqCst) {
                    let head = collect_head(key);
                    if Server::request_to(address, &head, &batch(n, 1)).0 == 202 {
                        acked.push(n);
                    }
                    n += 1;
                }
                (n, acked)
            });
            // 300 ms to 1.5 s, different every round, so the kill lands in
            // different places of the append, sync, insert and commit cycle.
            std::thread::sleep(Duration::from_millis(300 + u64::from(round) * 240));
            let mut server = server;
            let _ = server.child.kill();
            let _ = server.child.wait();
            stop.store(true, Ordering::SeqCst);
            sender.join().expect("sender")
        });
        next = last;
        acknowledged.extend(acked);
    }
    assert!(acknowledged.len() > 50, "the sender got events through");

    let server = Server::start_with(in_clickhouse(&dir, &database.name));
    let stored = |database: &ScratchDatabase| -> std::collections::BTreeSet<u32> {
        database
            .text("SELECT event_id FROM events FORMAT TSV")
            .lines()
            .filter_map(|line| line.rsplit('-').next()?.parse().ok())
            .collect()
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let have = stored(&database);
        if acknowledged.is_subset(&have) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{} acknowledged events never arrived",
            acknowledged.difference(&have).count()
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_millis(1500));
    let rows = database.number("SELECT count() FROM events");
    let unique = database.number("SELECT uniqExact(event_id) FROM events");
    eprintln!(
        "acknowledged {}, stored {unique} distinct in {rows} rows",
        acknowledged.len()
    );
    assert_eq!(
        stored(&database).len() as u64,
        unique,
        "every stored id was read back"
    );
    assert!(
        rows - unique <= u64::from(KILLS + 1) * 1000,
        "duplicates stay within one batch per restart"
    );
    drop(server);
}

fn identify_head(key: &str) -> String {
    format!("POST /v1/identify HTTP/1.1\r\nX-Dionaea-Key: {key}\r\nContent-Type: application/json")
}

fn identify_body(anon: &str, user: &str, traits: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis();
    format!(
        "{{\"sent_at\":{now},\"sdk\":\"test/1.0\",\"anon_id\":\"{anon}\",\"user_id\":\"{user}\",\"traits\":{traits}}}"
    )
}

#[test]
fn an_acknowledged_identify_is_in_the_spool_after_the_server_is_killed() {
    let dir = dir("identify-durable");
    let key = bootstrap_with(&dir, SCHEMA_WITH_TRAITS);
    let server = Server::start(&dir);

    let (status, response) = server.request(
        &identify_head(&key),
        &identify_body(
            "a1",
            "u_42",
            "{\"plan\":\"pro\",\"seats\":4,\"email\":\"x\"}",
        ),
    );
    assert_eq!(status, 202, "{response}");
    assert!(
        response.contains("{\"accepted\":1,\"stripped\":1}"),
        "{response}"
    );
    let (status, response) =
        server.request(&identify_head(&key), &identify_body("a1", "a@b.c", "{}"));
    assert_eq!(status, 400, "an email address is not a user id: {response}");
    assert_eq!(server.request("GET /v1/identify HTTP/1.1", "").0, 405);

    let metrics = server.ops("/metrics").1;
    assert_eq!(
        metric(&metrics, "dionaea_identifies_total{result=\"linked\"}"),
        1
    );
    assert_eq!(
        metric(&metrics, "dionaea_identify_traits_stripped_total"),
        1
    );

    drop(server);
    let mut config = SpoolConfig::new(dir.join("spool"));
    config.sync = false;
    let spool = Spool::open(config).expect("reopen the spool");
    let records = spool.read(100, usize::MAX).expect("read");
    assert_eq!(records.len(), 1, "the refused request wrote nothing");
    let text = String::from_utf8_lossy(&records[0].payload).into_owned();
    for expected in [
        "\"anon_id\":\"a1\"",
        "\"user_id\":\"u_42\"",
        "\"plan\":\"pro\"",
    ] {
        assert!(text.contains(expected), "{expected} in {text}");
    }
    assert!(!text.contains("email"), "an undeclared trait is not kept");
}

/// Identify against the whole product: the link and the traits reach
/// ClickHouse, the anonymous id's events show under the user, and a retry or
/// a later identify keeps one current row.
#[test]
#[ignore = "needs ClickHouse"]
fn identify_links_an_anonymous_id_to_a_user_and_keeps_the_latest_traits() {
    let database = ScratchDatabase::new("identify");
    let dir = dir("identify");
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
        std::fs::write(&schema, SCHEMA_WITH_TRAITS).expect("write the schema");
        run_here(&[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ]);
        run_here(&["key", "issue", "--site", "1"])
    };
    let server = Server::start_with(in_clickhouse(&dir, &database.name));

    // Events of two devices; only a1 is identified.
    assert_eq!(server.request(&collect_head(&key), &batch(1, 1)).0, 202);
    let other = batch(2, 1).replace("\"anon_id\":\"a1\"", "\"anon_id\":\"a2\"");
    assert_eq!(server.request(&collect_head(&key), &other).0, 202);
    let first = identify_body("a1", "u_42", "{\"plan\":\"free\",\"seats\":1}");
    assert_eq!(server.request(&identify_head(&key), &first).0, 202);
    // The same request again, as an SDK retry.
    assert_eq!(server.request(&identify_head(&key), &first).0, 202);

    database.wait_for("the events", "SELECT count() FROM events", 2);
    database.wait_for(
        "the link",
        "SELECT count() FROM identity_alias FINAL WHERE anon_id = 'a1' AND user_id = 'u_42'",
        1,
    );
    let of_user = "SELECT count() FROM events WHERE (site_id, anon_id) IN \
        (SELECT site_id, anon_id FROM identity_alias FINAL WHERE user_id = 'u_42')";
    assert_eq!(database.number(of_user), 1, "only the identified device");

    // Later traits replace the earlier ones; an identify with no declared
    // trait leaves them alone.
    std::thread::sleep(Duration::from_millis(20));
    let second = identify_body("a1", "u_42", "{\"plan\":\"pro\",\"seats\":5}");
    assert_eq!(server.request(&identify_head(&key), &second).0, 202);
    let none = identify_body("a1", "u_42", "{\"nothing_declared\":1}");
    assert_eq!(server.request(&identify_head(&key), &none).0, 202);
    let current = "SELECT concat(traits_str['plan'], ',', toString(traits_num['seats'])) \
        FROM user_traits FINAL WHERE user_id = 'u_42'";
    let deadline = Instant::now() + Duration::from_secs(60);
    while database.text(current) != "pro,5" {
        assert!(
            Instant::now() < deadline,
            "traits never became pro,5: {}",
            database.text(current)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        database.text(current),
        "pro,5",
        "the empty identify changed nothing"
    );
    assert_eq!(
        database.number("SELECT count() FROM identity_alias FINAL"),
        1,
        "one pair however often it was sent"
    );
    let metrics = server.ops("/metrics").1;
    assert!(metric(&metrics, "dionaea_batcher_rows_total{table=\"identity\"}") >= 2);
}

/// Erasure against the whole product: a user is erased from ClickHouse, what
/// belongs to anyone else stays, and the erased user and their device are
/// refused from then on.
#[test]
#[ignore = "needs ClickHouse"]
fn an_erased_user_disappears_from_clickhouse_and_stays_out() {
    let database = ScratchDatabase::new("erase");
    let dir = dir("erase");
    let cli = |args: &[&str]| {
        let output = in_clickhouse(&dir, &database.name)
            .args(args)
            .output()
            .expect("run the binary");
        assert!(output.status.success(), "{args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    cli(&["site", "create", "shop"]);
    let schema = dir.join("schema.toml");
    std::fs::write(&schema, SCHEMA_WITH_TRAITS).expect("write the schema");
    cli(&[
        "schema",
        "push",
        "--site",
        "1",
        schema.to_str().expect("utf-8"),
    ]);
    let key = cli(&["key", "issue", "--site", "1"]);

    let mut command = in_clickhouse(&dir, &database.name);
    command
        .env("DIONAEA_REFRESH_SECS", "1")
        .env("DIONAEA_ERASE_GRACE_SECS", "0")
        .env("DIONAEA_ERASE_FINISH_AFTER_SECS", "0");
    let server = Server::start_with(command);

    // Two people: u_gone on device a1, u_stays on device a2.
    let event_of = |n: u32, anon: &str| {
        batch(n, 1).replace("\"anon_id\":\"a1\"", &format!("\"anon_id\":\"{anon}\""))
    };
    for (n, anon) in [(1, "a1"), (2, "a1"), (3, "a2")] {
        assert_eq!(
            server.request(&collect_head(&key), &event_of(n, anon)).0,
            202
        );
    }
    for (anon, user) in [("a1", "u_gone"), ("a2", "u_stays")] {
        let body = identify_body(anon, user, "{\"plan\":\"pro\"}");
        assert_eq!(server.request(&identify_head(&key), &body).0, 202);
    }
    database.wait_for("the events", "SELECT count() FROM events", 3);
    database.wait_for("the links", "SELECT count() FROM identity_alias", 2);
    database.wait_for("the traits", "SELECT count() FROM user_traits", 2);

    let asked = cli(&[
        "erase", "request", "--site", "1", "--user", "u_gone", "--by", "test",
    ]);
    assert!(asked.contains("requested"), "{asked}");

    let deadline = Instant::now() + Duration::from_secs(90);
    while !cli(&["erase", "list", "--state", "complete"]).contains("complete") {
        assert!(
            Instant::now() < deadline,
            "never completed: {}",
            cli(&["erase", "show", "1"])
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let shown = cli(&["erase", "show", "1"]);
    assert!(shown.contains("verified"), "{shown}");
    assert!(!shown.contains("u_gone"), "the id is not printed");

    assert_eq!(database.number("SELECT count() FROM events"), 1, "only a2");
    assert_eq!(
        database.text("SELECT anon_id FROM events"),
        "a2",
        "the other person's event"
    );
    assert_eq!(
        database.text("SELECT user_id FROM identity_alias FINAL"),
        "u_stays"
    );
    assert_eq!(
        database.text("SELECT user_id FROM user_traits FINAL"),
        "u_stays"
    );
    assert_eq!(
        database.number(
            "SELECT count() FROM events WHERE anon_id = 'a1' SETTINGS apply_deleted_mask = 0"
        ),
        0,
        "gone from disk, not only hidden"
    );

    // From now on the user and the device are refused, the other stays welcome.
    let (status, answer) = server.request(&collect_head(&key), &event_of(10, "a1"));
    assert_eq!(status, 202);
    assert!(
        answer.contains("{\"accepted\":0,\"dropped\":1}"),
        "{answer}"
    );
    let (_, answer) = server.request(
        &identify_head(&key),
        &identify_body("a_new_device", "u_gone", "{}"),
    );
    assert!(
        answer.contains("{\"accepted\":0,\"stripped\":0}"),
        "{answer}"
    );
    assert_eq!(
        server.request(&collect_head(&key), &event_of(11, "a2")).0,
        202
    );
    database.wait_for(
        "the other person's new event",
        "SELECT count() FROM events",
        2,
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        database.number("SELECT count() FROM events WHERE anon_id = 'a1'"),
        0
    );
    let metrics = server.ops("/metrics").1;
    assert_eq!(
        metric(&metrics, "dionaea_identifies_total{result=\"erased\"}"),
        1
    );
}
