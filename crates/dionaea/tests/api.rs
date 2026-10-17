//! The API of the real binary against a real ClickHouse: events go in through
//! `/v1/collect`, members and tokens are made on the command line, and the
//! reports come out of `/api/v1/` over HTTP. Ignored by default;
//! `cargo test -p dionaea --test api -- --ignored` runs it against the
//! compose file's ClickHouse.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dionaea_store::{Config, Store};
use serde_json::{Value, json};

const SCHEMA: &str =
    "[events.checkout.fields.items]\nkind = \"count\"\nmax = 100\nrequired = true\n";
const PASSWORD: &str = "correct horse battery";

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("api")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn command(dir: &Path, database: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    command
        .env("DIONAEA_METADATA_PATH", dir.join("meta.db"))
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

struct Scratch {
    admin: Store,
    database: String,
}

impl Scratch {
    fn new() -> Self {
        let database = format!("dionaea_api_{}", std::process::id());
        let mut config = Config::new("127.0.0.1", 8123, "default");
        config.user = "dionaea".to_owned();
        config.password = "dionaea-dev".to_owned();
        let admin = Store::new(config).expect("a valid config");
        admin
            .raw_query(&format!("CREATE DATABASE IF NOT EXISTS {database}"))
            .expect("create the database");
        Self { admin, database }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self
            .admin
            .raw_query(&format!("DROP DATABASE IF EXISTS {}", self.database));
    }
}

struct Server {
    child: Child,
    collect: String,
    api: String,
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
        let (collect, api) = loop {
            let line = lines.next().expect("logs").expect("a line");
            if line.contains("\"message\":\"listening\"") {
                let value: Value = serde_json::from_str(&line).expect("json log");
                let field =
                    |name: &str| value["fields"][name].as_str().expect("address").to_owned();
                break (field("listen"), field("api"));
            }
        };
        std::thread::spawn(move || for _ in lines.map_while(Result::ok) {});
        Self {
            child,
            collect,
            api,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

fn http(address: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> Reply {
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
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

fn api(server: &Server, method: &str, path: &str, auth: &Auth, body: Option<Value>) -> Reply {
    let mut extra: Vec<(&str, String)> = Vec::new();
    match auth {
        Auth::None => {}
        Auth::Cookie(cookie) => extra.push(("Cookie", cookie.clone())),
        Auth::Bearer(token) => extra.push(("Authorization", format!("Bearer {token}"))),
    }
    let text = body.map(|b| b.to_string());
    if text.is_some() {
        extra.push(("Content-Type", "application/json".to_owned()));
    }
    let pairs: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http(
        &server.api,
        method,
        &format!("/api/v1/{path}"),
        &pairs,
        text.as_deref().unwrap_or(""),
    )
}

enum Auth {
    None,
    Cookie(String),
    Bearer(String),
}

fn login(server: &Server, email: &str) -> Auth {
    let reply = api(
        server,
        "POST",
        "login",
        &Auth::None,
        Some(json!({ "email": email, "password": PASSWORD })),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let cookie = reply.header("set-cookie").expect("a cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    Auth::Cookie(cookie.split(';').next().expect("pair").to_owned())
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis(),
    )
    .expect("fits")
}

fn event(n: u32, name: &str, anon: &str, session: &str, props: &Value) -> Value {
    json!({
        "event_id": format!("01890000-0000-7000-8000-{n:012}"),
        "ts": now_ms(),
        "name": name,
        "anon_id": anon,
        "session_id": session,
        "screen": "home",
        "props": props,
    })
}

#[test]
#[ignore = "needs ClickHouse"]
fn reports_come_out_of_the_api_for_members_and_tokens_and_stay_inside_their_site() {
    let scratch = Scratch::new();
    let db = scratch.database.as_str();
    let dir = dir("reports");
    let step = |args: &[&str], stdin: Option<&str>| run(&dir, db, args, stdin);

    step(&["site", "create", "shop", "--organization", "Acme"], None);
    step(&["site", "create", "blog", "--organization", "Acme"], None);
    let schema = dir.join("schema.toml");
    std::fs::write(&schema, SCHEMA).expect("write the schema");
    step(
        &[
            "schema",
            "push",
            "--site",
            "1",
            schema.to_str().expect("utf-8"),
        ],
        None,
    );
    let key = step(&["key", "issue", "--site", "1"], None);
    step(
        &[
            "member",
            "create",
            "owner@example.com",
            "--name",
            "Owner",
            "--organization",
            "Acme",
        ],
        Some(PASSWORD),
    );
    step(
        &[
            "member",
            "create",
            "viewer@example.com",
            "--name",
            "Viewer",
            "--organization",
            "Acme",
            "--role",
            "viewer",
        ],
        Some(PASSWORD),
    );
    let shop_token = step(&["token", "issue", "--site", "1", "--label", "shop"], None)
        .split('\t')
        .nth(1)
        .expect("a token")
        .to_owned();
    let blog_token = step(&["token", "issue", "--site", "2", "--label", "blog"], None)
        .split('\t')
        .nth(1)
        .expect("a token")
        .to_owned();

    let server = Server::start(&dir, db);

    // Three devices: two sessions of the first, one of each of the others.
    let events: Vec<Value> = vec![
        event(1, "screen", "a1", "s1", &json!({ "name": "home" })),
        event(
            2,
            "ui_tap",
            "a1",
            "s1",
            &json!({ "screen": "home", "element": "buy" }),
        ),
        event(3, "screen", "a1", "s1", &json!({ "name": "cart" })),
        event(4, "checkout", "a1", "s1", &json!({ "items": 2 })),
        event(5, "screen", "a2", "s2", &json!({ "name": "home" })),
        event(6, "screen", "a3", "s3", &json!({ "name": "home" })),
        event(
            7,
            "ui_tap",
            "a3",
            "s3",
            &json!({ "screen": "home", "element": "buy" }),
        ),
    ];
    let batch = json!({ "sent_at": now_ms(), "sdk": "test/1.0", "events": events }).to_string();
    let sent = http(
        &server.collect,
        "POST",
        "/v1/collect",
        &[
            ("X-Dionaea-Key", key.as_str()),
            ("Content-Type", "application/json"),
        ],
        &batch,
    );
    assert_eq!(sent.status, 202, "{}", sent.body);

    let owner = login(&server, "owner@example.com");
    let viewer = login(&server, "viewer@example.com");
    let day = 86_400_000_u64;
    let range = json!({ "from_ms": now_ms() - day, "to_ms": now_ms() + day });
    let query = |report: Value| {
        let mut body = range.clone();
        body["report"] = report;
        body
    };

    // Wait for the events to be stored: the report itself says when.
    let deadline = Instant::now() + Duration::from_secs(60);
    let overview = loop {
        let reply = api(
            &server,
            "POST",
            "sites/1/query",
            &owner,
            Some(query(json!({ "type": "overview" }))),
        );
        // Until the tables exist the store cannot be read: 503.
        assert!(matches!(reply.status, 200 | 503), "{}", reply.body);
        if reply.status == 200 && reply.json()["events"]["value"] == 7.0 {
            break reply.json();
        }
        assert!(
            Instant::now() < deadline,
            "the events did not arrive: {}",
            reply.body
        );
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(overview["users"]["value"], 3.0);
    assert_eq!(overview["sessions"]["sessions"]["value"], 3.0);
    assert_eq!(overview["sessions"]["bounces"]["value"], 1.0);

    // The reply is JSON, never cached.
    let raw = api(
        &server,
        "POST",
        "sites/1/query",
        &owner,
        Some(query(json!({ "type": "overview" }))),
    );
    assert_eq!(raw.header("content-type"), Some("application/json"));
    assert_eq!(raw.header("cache-control"), Some("no-store"));

    // Per-screen and per-element usage, top lists and a series.
    let screens = api(
        &server,
        "POST",
        "sites/1/query",
        &owner,
        Some(query(json!({ "type": "screens", "limit": null }))),
    );
    assert_eq!(screens.json()["rows"][0]["screen"], "home");
    assert_eq!(screens.json()["rows"][0]["views"], 3);
    assert_eq!(screens.json()["rows"][0]["taps"], 2);
    let elements = api(
        &server,
        "POST",
        "sites/1/query",
        &owner,
        Some(query(
            json!({ "type": "elements", "screen": null, "limit": null }),
        )),
    );
    assert_eq!(elements.json()["rows"][0]["element"], "buy");
    let top = api(
        &server,
        "POST",
        "sites/1/query",
        &owner,
        Some(query(
            json!({ "type": "top", "dimension": { "dimension": "event" }, "measure": "events", "event": null, "limit": 2 }),
        )),
    );
    assert_eq!(top.json()["rows"][0]["key"], "screen");
    assert_eq!(top.json()["rows"][0]["value"], 4.0);

    // Days follow the site's time zone, which only an administrator sets.
    let zone = json!({ "time_zone": "Pacific/Kiritimati" });
    assert_eq!(
        api(
            &server,
            "PUT",
            "sites/1/time-zone",
            &viewer,
            Some(zone.clone())
        )
        .status,
        403
    );
    assert_eq!(
        api(
            &server,
            "PUT",
            "sites/1/time-zone",
            &owner,
            Some(json!({ "time_zone": "Mars/Base" }))
        )
        .status,
        400
    );
    assert_eq!(
        api(&server, "PUT", "sites/1/time-zone", &owner, Some(zone)).status,
        204
    );
    let series = api(
        &server,
        "POST",
        "sites/1/query",
        &owner,
        Some(query(
            json!({ "type": "series", "measure": "events", "event": null, "granularity": "day", "breakdown": null }),
        )),
    );
    assert_eq!(series.status, 200, "{}", series.body);
    let total: f64 = series.json()["points"]
        .as_array()
        .expect("points")
        .iter()
        .map(|p| p["value"].as_f64().unwrap_or(0.0))
        .sum();
    assert_eq!(total, 7.0);

    // What retention would remove is counted, not deleted.
    let preview = api(
        &server,
        "GET",
        "sites/1/retention/preview?days=1",
        &owner,
        None,
    );
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert_eq!(preview.json()["days"], 1);
    assert_eq!(preview.json()["tables"][0]["table"], "events");
    assert_eq!(preview.json()["tables"][0]["rows"], 0);

    // A viewer reads, and cannot administer.
    let as_viewer = api(
        &server,
        "POST",
        "sites/1/query",
        &viewer,
        Some(query(json!({ "type": "overview" }))),
    );
    assert_eq!(as_viewer.status, 200);
    assert_eq!(
        api(&server, "GET", "sites/1/keys", &viewer, None).status,
        403
    );
    assert_eq!(
        api(&server, "GET", "sites/1/keys", &owner, None).status,
        200
    );

    // A token reads the site it was issued for and nothing else.
    let shop = Auth::Bearer(shop_token);
    let blog = Auth::Bearer(blog_token);
    let with_token = api(
        &server,
        "POST",
        "sites/1/query",
        &shop,
        Some(query(json!({ "type": "overview" }))),
    );
    assert_eq!(with_token.status, 200, "{}", with_token.body);
    assert_eq!(with_token.json()["users"]["value"], 3.0);
    assert_eq!(
        api(
            &server,
            "POST",
            "sites/2/query",
            &shop,
            Some(query(json!({ "type": "overview" })))
        )
        .status,
        404
    );
    assert_eq!(
        api(
            &server,
            "POST",
            "sites/1/query",
            &blog,
            Some(query(json!({ "type": "overview" })))
        )
        .status,
        404
    );
    assert_eq!(api(&server, "GET", "sites/1/keys", &shop, None).status, 403);
    // The blog has no events: its token sees nothing of the shop's.
    let own = api(
        &server,
        "POST",
        "sites/2/query",
        &blog,
        Some(query(json!({ "type": "overview" }))),
    );
    assert_eq!(own.json()["events"]["value"], 0.0);

    // The typed errors reach the caller.
    let undeclared = api(
        &server,
        "POST",
        "sites/1/query",
        &owner,
        Some(query(
            json!({ "type": "series", "measure": "events", "event": "nope", "granularity": "day", "breakdown": null }),
        )),
    );
    assert_eq!(
        (
            undeclared.status,
            undeclared.json()["error"]["code"].as_str()
        ),
        (400, Some("undeclared_event"))
    );
    let mut wide = range.clone();
    wide["from_ms"] = json!(0);
    wide["report"] = json!({ "type": "overview" });
    let too_wide = api(&server, "POST", "sites/1/query", &owner, Some(wide));
    assert_eq!(too_wide.json()["error"]["code"], "range_too_large");

    // A body past the limit is refused before it is read in full.
    let huge = "x".repeat(2 << 20);
    let refused = http(
        &server.api,
        "POST",
        "/api/v1/sites/1/query",
        &[("Content-Type", "application/json")],
        &huge,
    );
    assert!(matches!(refused.status, 413 | 0), "{}", refused.status);

    // Logging out ends the session for good.
    let Auth::Cookie(cookie) = &owner else {
        unreachable!()
    };
    let out = api(&server, "POST", "logout", &owner, None);
    assert_eq!(out.status, 204);
    assert_eq!(
        api(&server, "GET", "me", &Auth::Cookie(cookie.clone()), None).status,
        401
    );
}
