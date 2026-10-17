//! `dionaea symbols upload` against the real server: an upload token made on
//! the command line, a release's files sent from a directory, and what the
//! server then lists and keeps on disk. ClickHouse is not needed: the port is
//! closed, and uploading does not touch it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const PASSWORD: &str = "correct horse battery";

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("symbols")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn command(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    command
        .env("DIONAEA_METADATA_PATH", dir.join("meta.db"))
        .env("DIONAEA_ARTIFACTS_DIR", dir.join("artifacts"))
        .env("DIONAEA_ERASURE_KEY", "0123456789abcdef0123456789abcdef")
        .env("DIONAEA_SPOOL_DIR", dir.join("spool"))
        .env("DIONAEA_CLICKHOUSE_URL", "http://127.0.0.1:1")
        .env("DIONAEA_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_OPS_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_API_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_SECURE_COOKIES", "false")
        .env("DIONAEA_LOG_FORMAT", "json");
    command
}

fn run(dir: &Path, args: &[&str], stdin: Option<&str>) -> String {
    let mut child = command(dir)
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
    api: String,
}

impl Server {
    fn start(dir: &Path) -> Self {
        let mut child = command(dir)
            .arg("serve")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the server");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let api = loop {
            let line = lines.next().expect("logs").expect("a line");
            if line.contains("\"message\":\"listening\"") {
                let value: Value = serde_json::from_str(&line).expect("json log");
                break value["fields"]["api"].as_str().expect("address").to_owned();
            }
        };
        std::thread::spawn(move || for _ in lines.map_while(Result::ok) {});
        Self { child, api }
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

fn upload(
    dir: &Path,
    server: &Server,
    token: &str,
    release: &str,
    kind: &str,
    root: Option<&Path>,
    files: &[&Path],
) -> std::process::Output {
    let url = format!("http://{}", server.api);
    let mut cmd = command(dir);
    cmd.env("DIONAEA_UPLOAD_TOKEN", token).args([
        "symbols",
        "upload",
        "--url",
        &url,
        "--site",
        "1",
        "--release",
        release,
        "--kind",
        kind,
    ]);
    if let Some(root) = root {
        cmd.arg("--root").arg(root);
    }
    cmd.args(files).output().expect("run the upload")
}

#[test]
fn files_uploaded_from_a_build_directory_are_listed_replaced_and_removed() {
    let dir = dir("flow");
    run(
        &dir,
        &["site", "create", "shop", "--organization", "Acme"],
        None,
    );
    run(
        &dir,
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
    let issued = run(&dir, &["token", "issue", "--site", "1"], None);
    let read_token = issued.split('\t').nth(1).expect("a read token").to_owned();

    let server = Server::start(&dir);
    let json_of = |body: &str| -> Value { serde_json::from_str(body).unwrap_or(Value::Null) };

    // An administrator logs in and issues the upload token.
    let (status, body, cookies) = http(
        &server.api,
        "POST",
        "/api/v1/login",
        &[("Content-Type", "application/json")],
        &json!({ "email": "admin@example.com", "password": PASSWORD }).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    let cookie = cookies[0].split(';').next().expect("pair").to_owned();
    let as_admin = |method: &str, path: &str, body: Option<Value>| {
        let text = body.map(|b| b.to_string()).unwrap_or_default();
        let mut extra = vec![("Cookie", cookie.as_str())];
        if !text.is_empty() {
            extra.push(("Content-Type", "application/json"));
        }
        http(&server.api, method, path, &extra, &text)
    };
    let (status, body, _) = as_admin(
        "POST",
        "/api/v1/sites/1/upload-tokens",
        Some(json!({ "label": "ci" })),
    );
    assert_eq!(status, 201, "{body}");
    let token = json_of(&body)["token"]
        .as_str()
        .expect("a token")
        .to_owned();

    // A build directory with two source maps, one in a subdirectory.
    let build = dir.join("build");
    std::fs::create_dir_all(build.join("static/js")).expect("dirs");
    let main = build.join("static/js/main.js.map");
    let vendor = build.join("vendor.js.map");
    std::fs::write(
        &main,
        r#"{"version":3,"sources":["a.js"],"mappings":"AAAA"}"#,
    )
    .expect("write");
    std::fs::write(
        &vendor,
        r#"{"version":3,"sources":["v.js"],"mappings":"BBBB"}"#,
    )
    .expect("write");

    let output = upload(
        &dir,
        &server,
        &token,
        "1.4.2+57",
        "sourcemap",
        Some(&build),
        &[&main, &vendor],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(printed.contains("static/js/main.js.map\t"), "{printed}");
    assert!(printed.contains("vendor.js.map\t"), "{printed}");

    let (status, body, _) = as_admin("GET", "/api/v1/sites/1/symbols?release=1.4.2%2B57", None);
    assert_eq!(status, 200, "{body}");
    let listed = json_of(&body);
    let names: Vec<&str> = listed["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["static/js/main.js.map", "vendor.js.map"]);

    // The bytes are on disk, under names that hold nothing of the upload.
    let mut on_disk = Vec::new();
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    walk(&dir.join("artifacts"), &mut on_disk);
    assert_eq!(on_disk.len(), 2);
    let paths = on_disk
        .iter()
        .map(|p| p.display().to_string())
        .collect::<String>();
    assert!(
        !paths.contains("main") && !paths.contains("1.4.2"),
        "{paths}"
    );

    // A wrong kind of token, a wrong token and a bad file are refused.
    let refused = upload(
        &dir,
        &server,
        &read_token,
        "1.4.2+57",
        "sourcemap",
        Some(&build),
        &[&main],
    );
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let nonsense = dir.join("nonsense.map");
    std::fs::write(&nonsense, "not json").expect("write");
    let bad = upload(
        &dir,
        &server,
        &token,
        "1.4.2+57",
        "sourcemap",
        None,
        &[&nonsense],
    );
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("422"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
    let missing = upload(
        &dir,
        &server,
        &token,
        "1.4.2+57",
        "sourcemap",
        None,
        &[&dir.join("absent.map")],
    );
    assert_eq!(missing.status.code(), Some(2));

    // Removing the release removes its bytes too.
    let (status, _, _) = as_admin("DELETE", "/api/v1/sites/1/symbols?release=1.4.2%2B57", None);
    assert_eq!(status, 204);
    let mut left = Vec::new();
    walk(&dir.join("artifacts"), &mut left);
    assert!(left.is_empty(), "{left:?}");
}
