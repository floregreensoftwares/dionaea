//! Performance sampling with the real binary: a site that keeps half of its
//! performance events keeps about half, loses no error, says what each kept
//! event stands for, and goes back to keeping everything when told to. Needs
//! ClickHouse on 127.0.0.1:8123 (`docker compose up -d`).

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
        .join("sampling")
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
        .env("DIONAEA_REFRESH_SECS", "1")
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

fn api(
    server: &Server,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<Value>,
) -> (u16, String) {
    let text = body.map(|b| b.to_string()).unwrap_or_default();
    let (status, answer, _) = http(
        &server.api,
        method,
        path,
        &[("Cookie", cookie), ("Content-Type", "application/json")],
        &text,
    );
    (status, answer)
}

/// Sends `count` screen loads, numbered from `first`, 100 to a batch.
fn send_loads(server: &Server, key: &str, first: u32, count: u32) {
    let agent = "Mozilla/5.0 Chrome/120.0 Safari/537.36";
    let events: Vec<Value> = (first..first + count)
        .map(|n| raw_event(n, "screen_load", json!({ "render": 100 + n % 50 })))
        .collect();
    for chunk in events.chunks(100) {
        collect(server, key, agent, "1.0.0", chunk);
    }
}

#[test]
#[ignore = "needs ClickHouse"]
fn a_site_that_keeps_half_of_its_performance_events_loses_no_error_and_can_go_back() {
    let database = format!("dionaea_sampling_{}", std::process::id());
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
    // The command line says the same as the API will: everything is kept.
    assert_eq!(run(&dir, &database, &["sampling", "list"], None), "1\t1");

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

    let (status, body) = api(
        &server,
        "PUT",
        "/api/v1/sites/1/sampling",
        &cookie,
        Some(json!({ "performance": 0.5 })),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(run(&dir, &database, &["sampling", "list"], None), "1\t0.5");
    // The server applies it at its next refresh (every second here).
    std::thread::sleep(Duration::from_millis(2_500));

    // 400 screen loads, 20 errors, and an event the site declared.
    send_loads(&server, &key, 1, 400);
    let agent = "Mozilla/5.0 Chrome/120.0 Safari/537.36";
    let mut others: Vec<Value> = (1001..1021)
        .map(|n| error_event(n, "https://app.example.com/a.js|fa|1|1|1"))
        .collect();
    others.push(raw_event(2001, "checkout", json!({ "items": 3 })));
    collect(&server, &key, agent, "1.0.0", &others);

    let store = make(&database);
    let number = |sql: &str| -> u64 {
        store
            .raw_query(sql)
            .map(|text| text.trim().parse().unwrap_or(0))
            .unwrap_or(0)
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    // Every event is either stored or counted as sampled out.
    loop {
        let kept = number("SELECT count() FROM events WHERE name = 'screen_load'");
        let counted = number(
            "SELECT sum(count) FROM drops_daily WHERE reason = 'sampled' AND event_name = 'screen_load'",
        );
        if kept + counted == 400 && number("SELECT count() FROM errors") == 20 {
            break;
        }
        assert!(Instant::now() < deadline, "kept {kept}, counted {counted}");
        std::thread::sleep(Duration::from_millis(300));
    }
    let kept = number("SELECT count() FROM events WHERE name = 'screen_load'");
    // Four standard deviations of a binomial count of 400 at one half.
    assert!((160..=240).contains(&kept), "{kept} kept of 400");
    assert_eq!(
        number("SELECT count() FROM errors"),
        20,
        "no error is sampled"
    );
    assert_eq!(
        number("SELECT count() FROM events WHERE name = 'checkout'"),
        1,
        "a declared event is not sampled"
    );
    assert_eq!(
        number("SELECT count() FROM measurements WHERE sample_rate = 0.5"),
        kept
    );
    let metrics = http(&server.ops, "GET", "/metrics", &[], "").1;
    let line = metrics
        .lines()
        .find(|l| l.starts_with("dionaea_events_dropped_total{reason=\"sampled\"}"))
        .expect("the sampled drops are counted by reason");
    let counted: u64 = line
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a number");
    assert_eq!(counted, 400 - kept);
    // What the kept events stand for adds back to about what was sent.
    let stands_for = number("SELECT toUInt64(round(sum(weight))) FROM perf_daily");
    assert_eq!(stands_for, kept * 2);

    // Back to keeping everything: the next 100 are all stored, with no rate.
    let (status, _) = api(&server, "DELETE", "/api/v1/sites/1/sampling", &cookie, None);
    assert_eq!(status, 204);
    assert_eq!(run(&dir, &database, &["sampling", "list"], None), "1\t1");
    std::thread::sleep(Duration::from_millis(2_500));
    send_loads(&server, &key, 5001, 100);
    let deadline = Instant::now() + Duration::from_secs(60);
    while number("SELECT count() FROM events WHERE name = 'screen_load'") < kept + 100 {
        assert!(Instant::now() < deadline, "the last 100 did not arrive");
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(
        number("SELECT count() FROM measurements WHERE sample_rate = 1"),
        100
    );
}
