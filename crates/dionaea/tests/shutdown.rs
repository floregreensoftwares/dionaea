//! Starts the real binary and stops it with SIGTERM, the signal orchestrators
//! send. Unix only: Windows has no SIGTERM, and Ctrl+C there needs a console.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn sigterm_stops_the_server_after_an_orderly_shutdown() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dionaea"))
        .arg("serve")
        .env("DIONAEA_LISTEN", "127.0.0.1:0")
        .env("DIONAEA_METADATA_PATH", dir.join("meta.db"))
        .env("DIONAEA_SPOOL_DIR", dir.join("spool"))
        .env("DIONAEA_LOG_FORMAT", "json")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    assert!(first.contains("\"message\":\"listening\""), "got: {first}");

    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("the server did not exit within 10 seconds of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(exit.success(), "exit status: {exit}");

    let rest: Vec<String> = lines.map(Result::unwrap).collect();
    let rest = rest.join("\n");
    assert!(rest.contains("terminate received"), "got: {rest}");
    assert!(rest.contains("shutdown complete"), "got: {rest}");
}
