//! Error stacks translated on their way to ClickHouse, with the real binary:
//! the esbuild bundle's source map is uploaded with `dionaea symbols upload`,
//! error events are collected over HTTP, and the stored rows are read back
//! from ClickHouse. Needs ClickHouse on 127.0.0.1:8123 (`docker compose up -d`).

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
const RELEASE: &str = "1.4.2+57";
const MAPPING: &str = "\
com.example.Cart -> a.a:
    12:15:void checkout(int):40:43 -> a
";

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("symbolication")
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

#[test]
#[ignore = "needs ClickHouse"]
fn error_stacks_are_translated_on_their_way_to_clickhouse() {
    let database = format!("dionaea_symbolicate_{}", std::process::id());
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
    let (status, body, _) = http(
        &server.api,
        "POST",
        "/api/v1/sites/1/upload-tokens",
        &[("Cookie", &cookie), ("Content-Type", "application/json")],
        &json!({ "label": "ci" }).to_string(),
    );
    assert_eq!(status, 201, "{body}");
    let token = serde_json::from_str::<Value>(&body).expect("json")["token"]
        .as_str()
        .expect("a token")
        .to_owned();

    // The files of a build: the real bundle's source map, and a mapping.
    let build = dir.join("build");
    std::fs::create_dir_all(&build).expect("dirs");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../dionaea-health/tests/fixtures/js/app.min.js.map");
    let map = build.join("app.min.js.map");
    std::fs::copy(&fixture, &map).expect("copy the fixture");
    let mapping = build.join("mapping.txt");
    std::fs::write(&mapping, MAPPING).expect("write the mapping");
    let url = format!("http://{}", server.api);
    for (kind, file) in [("sourcemap", &map), ("proguard", &mapping)] {
        let output = command(&dir, &database)
            .env("DIONAEA_UPLOAD_TOKEN", &token)
            .args([
                "symbols",
                "upload",
                "--url",
                &url,
                "--site",
                "1",
                "--release",
                RELEASE,
                "--kind",
                kind,
                "--root",
            ])
            .arg(&build)
            .arg(file)
            .output()
            .expect("run the upload");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let minified = [56, 158, 134, 240, 311, 368]
        .map(|column| format!("https://app.example.com/app.min.js|x|1|{column}|1"))
        .join(";");
    // The event ids: 1 translated, 2 for a release nobody uploaded, 3 Android.
    collect(
        &server,
        &key,
        "Mozilla/5.0 Chrome/120.0 Safari/537.36",
        RELEASE,
        &[error_event(1, &minified)],
    );
    collect(
        &server,
        &key,
        "Mozilla/5.0 Chrome/120.0 Safari/537.36",
        "9.9.9",
        &[error_event(2, &minified)],
    );
    collect(
        &server,
        &key,
        "okhttp/4.12 Dalvik/2.1.0",
        RELEASE,
        &[error_event(3, "a.a|a|13||1")],
    );

    let store = make(&database);
    let read = |id: u32| -> Vec<String> {
        let sql = format!(
            "SELECT props_str['symbols'], props_str['frames'], props_str['fingerprint'] \
             FROM events WHERE event_id = '01890000-0000-7000-8000-{id:012}' FORMAT TSVRaw"
        );
        store
            .raw_query(&sql)
            .map(|text| text.trim().split('\t').map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while [1, 2, 3].iter().any(|id| read(*id).len() < 3) {
        assert!(Instant::now() < deadline, "the events did not arrive");
        std::thread::sleep(Duration::from_millis(200));
    }

    let first = read(1);
    assert_eq!(first[0], "ok");
    let frames: Vec<&str> = first[1].split(';').collect();
    assert_eq!(frames.len(), 6);
    assert!(
        frames[0].ends_with("util.js|parseQuantity|4|11|1"),
        "{}",
        frames[0]
    );
    assert!(
        frames[3].ends_with("cart.js|checkout|9|20|1"),
        "{}",
        frames[3]
    );
    assert!(
        frames[4].ends_with("index.js|submitOrder|5|15|1"),
        "{}",
        frames[4]
    );
    assert_eq!(first[2].len(), 32);

    // No file for that release: stored as sent, flagged, still grouped.
    let second = read(2);
    assert_eq!(second[0], "missing");
    assert_eq!(second[1], minified);
    assert_eq!(second[2].len(), 32);
    assert_ne!(second[2], first[2], "raw and translated stacks group apart");

    let third = read(3);
    assert_eq!(third[0], "ok");
    assert_eq!(third[1], "com.example.Cart|checkout|41||1");

    // The same rows are in the errors table, with their own columns.
    let errors = store
        .raw_query(
            "SELECT symbols, fingerprint = '' FROM errors ORDER BY event_id FORMAT TabSeparated",
        )
        .expect("errors");
    assert_eq!(
        errors.lines().collect::<Vec<_>>(),
        ["ok\t0", "missing\t0", "ok\t0"]
    );
    let grouped = store
        .raw_query("SELECT uniqExact(fingerprint) FROM errors")
        .expect("fingerprints");
    assert_eq!(grouped.trim(), "3", "raw and translated stacks group apart");

    let metrics = http(&server.ops, "GET", "/metrics", &[], "").1;
    for expected in [
        "dionaea_symbolication_events_total{outcome=\"ok\"} 2",
        "dionaea_symbolication_events_total{outcome=\"missing\"} 1",
        "dionaea_symbolication_file_loads_total 2",
    ] {
        assert!(metrics.contains(expected), "{expected}\n{metrics}");
    }
}
