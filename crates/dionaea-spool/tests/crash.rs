//! Kills a real process while it writes to and reads from the spool, then
//! opens the directory again and checks what survived.
//!
//! The test binary starts itself: the `worker` test does nothing unless the
//! environment names a mode, and the tests below run it as a child process
//! and kill it (`TerminateProcess` on Windows, `SIGKILL` elsewhere) after a
//! number of acknowledgements that differs on every round.
//!
//! This proves that a process crash loses nothing acknowledged. It cannot
//! prove survival of a power cut, which depends on the disk honouring `fsync`.

// The worker talks to its parent through stdout, on purpose.
#![allow(clippy::print_stdout)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;

use dionaea_spool::{Config, Spool};

const WORKERS: u64 = 4;
const ID_SPACE: u64 = 1_000_000;

/// A payload that carries its own id and a check, so any damage is visible.
fn payload(id: u64) -> Vec<u8> {
    let filler = 16 + (id.wrapping_mul(2_654_435_761) % 1500) as usize;
    let mut bytes = Vec::with_capacity(filler);
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&id.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    bytes.resize(filler, (id % 251) as u8);
    bytes
}

/// The id in a payload that checks out.
fn verify(payload: &[u8]) -> Option<u64> {
    if payload.len() < 16 {
        return None;
    }
    let id = u64::from_le_bytes(payload[..8].try_into().ok()?);
    (payload == payload_of(id).as_slice()).then_some(id)
}

fn payload_of(id: u64) -> Vec<u8> {
    payload(id)
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("dionaea-spool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &Path) -> Config {
    let mut config = Config::new(dir);
    // Small segments, so kills often land while one is being sealed and the next created.
    config.segment_bytes = 8 * 1024;
    config
}

// The child process

#[test]
#[ignore = "started as a child process by the crash tests"]
fn worker() {
    let Ok(mode) = std::env::var("SPOOL_WORKER_MODE") else {
        return;
    };
    let dir = PathBuf::from(std::env::var("SPOOL_DIR").expect("a directory"));
    let base: u64 = std::env::var("SPOOL_BASE")
        .expect("a base")
        .parse()
        .expect("a number");
    // The test harness has just printed "test worker ... " without a newline.
    println!();
    let spool = Spool::open(config(&dir)).expect("open");
    match mode.as_str() {
        "append" => {
            let threads: Vec<_> = (0..WORKERS)
                .map(|t| {
                    let spool = spool.clone();
                    thread::spawn(move || {
                        for i in 0..u64::MAX {
                            let id = base + t * ID_SPACE + i;
                            spool.append(payload(id)).expect("append");
                            println!("acked {id}");
                        }
                    })
                })
                .collect();
            for thread in threads {
                thread.join().expect("a worker thread");
            }
        }
        "consume" => loop {
            let records = spool.read(10, usize::MAX).expect("read");
            let Some(last) = records.last() else {
                println!("done");
                return;
            };
            for record in &records {
                let id = verify(&record.payload).expect("a payload that checks out");
                println!("got {id}");
            }
            let last_id = verify(&last.payload).expect("a payload that checks out");
            spool.commit(last.next).expect("commit");
            println!("committed {last_id}");
        },
        other => panic!("unknown mode {other}"),
    }
}

fn start_worker(mode: &str, dir: &Path, base: u64) -> Child {
    Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            "--exact",
            "worker",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SPOOL_WORKER_MODE", mode)
        .env("SPOOL_DIR", dir)
        .env("SPOOL_BASE", base.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the worker")
}

/// Reads the worker's output lines. When `stop` says so the worker is killed;
/// the lines it printed before dying, still in the pipe, are read too, so the
/// parent knows everything that was acknowledged or committed.
fn run_until(mut child: Child, mut stop: impl FnMut(&[String]) -> bool) -> Vec<String> {
    let stdout = child.stdout.take().expect("a piped stdout");
    let mut lines = Vec::new();
    let reader = BufReader::new(stdout).lines();
    let mut killed = false;
    for line in reader {
        lines.push(line.expect("a line"));
        if !killed && stop(&lines) {
            let _ = child.kill();
            killed = true;
        }
    }
    child.wait().expect("wait for the worker");
    lines
}

/// The ids in the lines that start with `prefix`.
fn ids_after(lines: &[String], prefix: &str) -> Vec<u64> {
    lines
        .iter()
        .filter_map(|line| line.strip_prefix(prefix)?.trim().parse().ok())
        .collect()
}

/// Kills the worker after `count` lines that start with `prefix`.
fn kill_after(child: Child, prefix: &str, count: usize) -> Vec<u64> {
    let lines = run_until(child, |lines| ids_after(lines, prefix).len() >= count);
    ids_after(&lines, prefix)
}

// Crash during appends

#[test]
fn nothing_acknowledged_is_lost_when_the_process_is_killed_while_appending() {
    let dir = TestDir::new("append");
    let mut acknowledged = BTreeSet::new();
    let rounds = 10u64;
    for round in 0..rounds {
        let base = round * WORKERS * ID_SPACE;
        let child = start_worker("append", &dir.0, base);
        // A different number of acknowledgements before each kill.
        let count = 20 + ((round * 53) % 150) as usize;
        acknowledged.extend(kill_after(child, "acked ", count));

        // Open the directory as a restarted process would, and check.
        let spool = Spool::open(config(&dir.0)).expect("reopen after a kill");
        let mut found = BTreeMap::new();
        for record in spool.read(1_000_000, usize::MAX).expect("read") {
            let id = verify(&record.payload).expect("every surviving record is intact");
            assert!(found.insert(id, ()).is_none(), "record {id} appears twice");
        }
        for id in &acknowledged {
            assert!(
                found.contains_key(id),
                "acknowledged record {id} was lost in round {round}"
            );
        }
        // What was found is at least what was acknowledged; the extra are
        // records written but not yet acknowledged when the kill came.
        assert!(found.len() >= acknowledged.len());
        // Per worker, what survived is a prefix of what it wrote: no gaps.
        for t in 0..WORKERS {
            let low = base + t * ID_SPACE;
            let own: Vec<u64> = found
                .keys()
                .copied()
                .filter(|id| (low..low + ID_SPACE).contains(id))
                .collect();
            assert!(
                own.iter().enumerate().all(|(i, id)| *id == low + i as u64),
                "a gap in worker {t}, round {round}"
            );
        }
        drop(spool);
    }
    assert!(
        acknowledged.len() > 500,
        "the rounds acknowledged only {}",
        acknowledged.len()
    );
}

// Crash during consumption

#[test]
fn a_kill_while_reading_and_committing_loses_nothing_and_replays_the_rest() {
    let dir = TestDir::new("consume");
    let total = 600u64;
    {
        let mut prefill = config(&dir.0);
        prefill.sync = false;
        let spool = Spool::open(prefill).expect("open");
        for id in 0..total {
            spool.append(payload(id)).expect("append");
        }
    }

    let mut delivered = BTreeSet::new();
    let mut last_committed: Option<u64> = None;
    for round in 0..8u64 {
        let child = start_worker("consume", &dir.0, 0);
        // Commit lines come after the matching `got` lines; stop after a few.
        let stdout_child = child;
        let lines = collect_lines(stdout_child, 25 + (round as usize * 17) % 70);
        for line in &lines {
            if let Some(id) = line.strip_prefix("got ") {
                delivered.insert(id.trim().parse::<u64>().expect("an id"));
            }
            if let Some(id) = line.strip_prefix("committed ") {
                last_committed = Some(id.trim().parse().expect("an id"));
            }
        }

        // After the kill, what is left is a contiguous suffix that starts
        // after the last commit the worker reported (the marker is durable
        // before the line is printed), and at most one batch further on.
        let spool = Spool::open(config(&dir.0)).expect("reopen after a kill");
        let ids: Vec<u64> = spool
            .read(1_000_000, usize::MAX)
            .expect("read")
            .iter()
            .map(|r| verify(&r.payload).expect("intact"))
            .collect();
        if let Some(&first) = ids.first() {
            assert!(
                ids.iter()
                    .enumerate()
                    .all(|(i, id)| *id == first + i as u64),
                "a gap after round {round}"
            );
            assert_eq!(
                *ids.last().expect("not empty"),
                total - 1,
                "the tail was lost in round {round}"
            );
            if let Some(committed) = last_committed {
                assert!(
                    first > committed,
                    "round {round}: {first} is not after commit {committed}"
                );
                assert!(
                    first <= committed + 1 + 10,
                    "round {round}: {first} is too far after {committed}"
                );
            }
        }
        drop(spool);
    }

    // Let a worker finish: every record must have been delivered at least once.
    let child = start_worker("consume", &dir.0, 0);
    for line in collect_lines(child, usize::MAX) {
        if let Some(id) = line.strip_prefix("got ") {
            delivered.insert(id.trim().parse::<u64>().expect("an id"));
        }
    }
    let missing: Vec<u64> = (0..total).filter(|id| !delivered.contains(id)).collect();
    assert!(missing.is_empty(), "never delivered: {missing:?}");
}

/// The worker's output, killing it after `limit` lines, or letting it finish
/// (it prints `done`).
fn collect_lines(child: Child, limit: usize) -> Vec<String> {
    run_until(child, |lines| lines.len() >= limit)
}
