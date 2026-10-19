//! Issues and health measurements with the real binary: error events are
//! collected, a person resolves an issue over the API, the same error comes
//! back in a later release and the regression job reopens it; performance
//! events become measurements and percentile states. Needs ClickHouse on
//! 127.0.0.1:8123 (`docker compose up -d`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dionaea_store::{Config, Store};
use serde_json::{Value, json};

const PASSWORD: &str = "correct horse battery";
const SCHEMA: &str =
    "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n";
fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("issues")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn command(dir: &Path, database: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    command
        .env("DIONAEA_METADATA_PATH", dir.join("meta.db"))
        .env("DIONAEA_ARTIFACTS_DIR", dir.join("artifacts"))
        .env("DIONAEA_ERASURE_KEY", "0123456789abcdef0123456789abcdef")
        .env("DIONAEA_SPOOL_DIR", dir.join("spool"))
        .env("DIONAEA_CLICKHOUSE_URL", "http://127.0.0.1:8123")
        .env("DIONAEA_CLICKHOUSE_USER", "dionaea")
        .env("DIONAEA_CLICKHOUSE_PASSWORD", "dionaea-dev")
        .env("DIONAEA_CLICKHOUSE_DATABASE", database)
        .env("DIONAEA_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_OPS_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_API_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_SECURE_COOKIES", "false")
        .env("DIONAEA_REGRESSION_INTERVAL_SECS", "30")
        .env("DIONAEA_LOG_FORMAT", "json");
    command
}

fn run(dir: &Path, database: &str, args: &[&str], stdin: Option<&str>) -> String {
    let mut child = command(dir, database)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the binary");
    {
        let mut input = child.stdin.take().expect("stdin");
        if let Some(text) = stdin {
            writeln!(input, "{text}").expect("write stdin");
        }
    }
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Server {
    child: Child,
    collect: String,
    api: String,
    ops: String,
}

impl Server {
    fn start(dir: &Path, database: &str) -> Self {
        let mut child = command(dir, database)
            .arg("serve")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the server");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let (collect, api, ops) = loop {
            let line = lines.next().expect("logs").expect("a line");
            if line.contains("\"message\":\"listening\"") {
                let value: Value = serde_json::from_str(&line).expect("json log");
                let field =
                    |name: &str| value["fields"][name].as_str().expect("address").to_owned();
                break (field("listen"), field("api"), field("ops"));
            }
        };
        std::thread::spawn(move || for _ in lines.map_while(Result::ok) {});
        Self {
            child,
            collect,
            api,
            ops,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(
    address: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> (u16, String, Vec<String>) {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).expect("write");
    let _ = stream.write_all(body.as_bytes());
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let cookies = head
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .or_else(|| l.strip_prefix("Set-Cookie: "))
        })
        .map(str::to_owned)
        .collect();
    (status, body.to_owned(), cookies)
}

fn error_event(n: u32, frames: &str) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis() as u64;
    json!({
        "event_id": format!("01890000-0000-7000-8000-{n:012}"),
        "ts": now,
        "name": "app_error",
        "anon_id": "a1",
        "props": {
            "kind": "unhandled",
            "type": "TypeError",
            "message": "bad quantity",
            "frames": frames,
        }
    })
}

fn collect(server: &Server, key: &str, user_agent: &str, release: &str, events: &[Value]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis() as u64;
    let body = json!({
        "sent_at": now,
        "sdk": "test/1.0",
        "release": release,
        "events": events,
    })
    .to_string();
    let (status, answer, _) = http(
        &server.collect,
        "POST",
        "/v1/collect",
        &[
            ("X-Dionaea-Key", key),
            ("Content-Type", "application/json"),
            ("User-Agent", user_agent),
        ],
        &body,
    );
    assert_eq!(status, 202, "{answer}");
}

fn raw_event(n: u32, name: &str, props: Value) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis() as u64;
    json!({
        "event_id": format!("01890000-0000-7000-8000-{n:012}"),
        "ts": now,
        "name": name,
        "anon_id": format!("dev{n}"),
        "props": props,
    })
}

fn issue_state(server: &Server, cookie: &str, fingerprint: &str) -> Value {
    let (status, body, _) = http(
        &server.api,
        "GET",
        &format!("/api/v1/sites/1/issues/{fingerprint}"),
        &[("Cookie", cookie)],
        "",
    );
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("json")
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_resolved_issue_reopens_when_its_error_returns_in_a_later_release() {
    let database = format!("dionaea_issues_{}", std::process::id());
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
                .raw_query(&format!("DROP DATABASE IF EXISTS {} SYNC", self.1));
        }
    }
    let _cleanup = Cleanup(admin, database.clone());

    let dir = dir("flow");
    run(
        &dir,
        &database,
        &["site", "create", "shop", "--organization", "Acme"],
        None,
    );
    let schema = dir.join("schema.toml");
    std::fs::write(&schema, SCHEMA).expect("write the schema");
    run(
        &dir,
        &database,
        &[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ],
        None,
    );
    let key = run(&dir, &database, &["key", "issue", "--site", "1"], None);
    run(
        &dir,
        &database,
        &[
            "member",
            "create",
            "admin@example.com",
            "--name",
            "Admin",
            "--organization",
            "Acme",
            "--role",
            "admin",
        ],
        Some(PASSWORD),
    );
    let server = Server::start(&dir, &database);
    let (status, body, cookies) = http(
        &server.api,
        "POST",
        "/api/v1/login",
        &[("Content-Type", "application/json")],
        &json!({ "email": "admin@example.com", "password": PASSWORD }).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    let cookie = cookies[0].split(';').next().expect("pair").to_owned();
    let agent = "Mozilla/5.0 Chrome/120.0 Safari/537.36";

    // Two different errors in release 1.0.0, and performance events.
    let frames_a = "https://app.example.com/a.js|fa|1|1|1";
    let frames_b = "https://app.example.com/b.js|fb|2|2|1";
    collect(
        &server,
        &key,
        agent,
        "1.0.0",
        &[
            error_event(1, frames_a),
            error_event(2, frames_b),
            raw_event(
                3,
                "app_start",
                json!({ "start": "cold", "first_frame": 800, "interactive": 1500 }),
            ),
            raw_event(
                4,
                "http_request",
                json!({
                    "method": "get", "host": "api.example.com", "route": "/orders",
                    "status": 200, "duration": 250
                }),
            ),
        ],
    );
    let store = make(&database);
    let wait_for = |sql: &str, expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let got = store.raw_query(sql).unwrap_or_default();
            if got.trim() == expected {
                return;
            }
            assert!(Instant::now() < deadline, "{sql}: got {got:?}");
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    wait_for("SELECT count() FROM errors", "2");
    wait_for("SELECT count() FROM measurements", "3");
    let fingerprint_of = |frames: &str| {
        store
            .raw_query(&format!(
                "SELECT fingerprint FROM errors WHERE frames = '{frames}' LIMIT 1"
            ))
            .expect("fingerprint")
            .trim()
            .to_owned()
    };
    let (fa, fb) = (fingerprint_of(frames_a), fingerprint_of(frames_b));
    assert_eq!((fa.len(), fb.len()), (32, 32));
    assert_ne!(fa, fb);

    // Measurements and the percentile rollup, from the real pipeline.
    wait_for(
        "SELECT arrayStringConcat(groupArray(metric), ',') FROM \
         (SELECT metric FROM measurements ORDER BY metric)",
        "app_start_first_frame,app_start_interactive,http_duration",
    );
    wait_for("SELECT sum(samples) FROM perf_daily", "3");
    wait_for("SELECT sum(events) FROM issues_daily", "2");

    // Both are resolved in 1.0.0. Resolving is by the second, and an error
    // only counts when it arrives after it.
    for fingerprint in [&fa, &fb] {
        let (status, body, _) = http(
            &server.api,
            "PUT",
            &format!("/api/v1/sites/1/issues/{fingerprint}"),
            &[("Cookie", &cookie), ("Content-Type", "application/json")],
            &json!({ "state": "resolved", "release": "1.0.0" }).to_string(),
        );
        assert_eq!(status, 200, "{body}");
    }
    std::thread::sleep(Duration::from_millis(1_500));

    // A returns in 1.1.0, a later release; B returns in 1.0.0 itself.
    collect(&server, &key, agent, "1.1.0", &[error_event(5, frames_a)]);
    collect(&server, &key, agent, "1.0.0", &[error_event(6, frames_b)]);
    wait_for("SELECT count() FROM errors", "4");

    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        if issue_state(&server, &cookie, &fa)["state"] == "unresolved" {
            break;
        }
        assert!(Instant::now() < deadline, "the issue did not reopen");
        std::thread::sleep(Duration::from_secs(1));
    }
    let reopened = issue_state(&server, &cookie, &fa);
    assert_eq!(reopened["regressed_release"], "1.1.0");
    assert_eq!(reopened["regressions"], 1);
    assert_eq!(reopened["resolved_release"], "1.0.0");
    assert_eq!(
        issue_state(&server, &cookie, &fb)["state"],
        "resolved",
        "an error in the release it was resolved in does not reopen it"
    );

    let metrics = http(&server.ops, "GET", "/metrics", &[], "").1;
    for expected in [
        "dionaea_regression_issues_reopened_total 1",
        "dionaea_regression_failing 0",
    ] {
        assert!(metrics.contains(expected), "{expected}\n{metrics}");
    }
}
