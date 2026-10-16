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
        let address = loop {
            let line = lines
                .next()
                .expect("the server logs before it exits")
                .expect("a line");
            if line.contains("\"message\":\"listening\"") {
                let value: serde_json::Value = serde_json::from_str(&line).expect("json log");
                break value["fields"]["listen"]
                    .as_str()
                    .expect("the address")
                    .to_owned();
            }
        };
        // Keep draining so the server never blocks writing its logs.
        std::thread::spawn(move || for _ in lines {});
        Self { child, address }
    }

    /// Sends one request and returns (status, headers and body as text).
    fn request(&self, head: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let request = format!(
            "{head}\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.address,
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
        4,
        "every acknowledged request is a durable record"
    );
    let text: String = records
        .iter()
        .map(|r| String::from_utf8_lossy(&r.payload).into_owned())
        .collect();
    assert!(text.contains("01890000-0000-7000-8000-000000000001"));
    assert!(
        text.contains("invalid_required_field"),
        "the drop count rode with its request"
    );
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
