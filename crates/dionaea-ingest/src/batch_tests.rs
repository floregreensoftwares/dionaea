//! The spool record format, the sink and the batcher, with a destination that
//! fails on demand and mimics ClickHouse's deduplication by token.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dionaea_core::DropReason;
use dionaea_spool::{Config as SpoolConfig, Spool};
use dionaea_store::{AliasRow, Error, TraitsRow};

use super::*;
use crate::sink;

const NOW: u64 = 1_760_000_000_000;
const POISON: &str = "POISON";

fn spool_dir(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "dionaea-ingest-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open(dir: &PathBuf) -> Spool {
    let mut config = SpoolConfig::new(dir);
    config.sync = false;
    config.max_record_bytes = 4 << 20;
    Spool::open(config).expect("spool")
}

fn accepted(n: u32, extra: usize) -> Accepted {
    Accepted {
        site_id: 7,
        event_id: format!("01890000-0000-7000-8000-{n:012}"),
        ts_ms: NOW,
        received_ms: NOW,
        name: "checkout".to_owned(),
        schema_version: 1,
        anon_id: "anon1".to_owned(),
        session_id: Some("s1".to_owned()),
        screen: None,
        strings: vec![("note".to_owned(), "x".repeat(extra))],
        numbers: vec![("items".to_owned(), 3.0)],
        app_version: Some("1.0".to_owned()),
        sdk: "t/1".to_owned(),
        event_key: u128::from(n),
        country: String::new(),
        platform: String::new(),
        os: String::new(),
        browser: String::new(),
        device_class: String::new(),
    }
}

fn request(events: Vec<Accepted>, drops: u64) -> Processed {
    Processed {
        accepted: events,
        drops: if drops == 0 {
            Vec::new()
        } else {
            vec![DropCount {
                event_name: "checkout".to_owned(),
                reason: Reason::Guard(DropReason::MissingRequiredField),
                count: drops,
            }]
        },
    }
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn fast() -> BatcherConfig {
    BatcherConfig {
        min_interval: Duration::ZERO,
        poll: Duration::from_millis(5),
        backoff_min: Duration::from_millis(5),
        backoff_max: Duration::from_millis(20),
        ..BatcherConfig::default()
    }
}

/// Stores rows like ClickHouse with deduplication on: a token seen before
/// stores nothing.
#[derive(Default)]
struct Fake {
    transient_failures: AtomicUsize,
    always_fail: std::sync::atomic::AtomicBool,
    tokens: Mutex<HashSet<String>>,
    attempts: Mutex<Vec<String>>,
    stored_events: Mutex<Vec<String>>,
    stored_drops: Mutex<Vec<String>>,
    stored_aliases: Mutex<Vec<String>>,
    stored_traits: Mutex<Vec<String>>,
    /// The ids other nodes (and this one) recorded as stored.
    known: Mutex<HashSet<(u32, String)>>,
    lookups_to_fail: AtomicUsize,
    lookup_rejected: std::sync::atomic::AtomicBool,
    ids_rejected: std::sync::atomic::AtomicBool,
    /// What happened to the store, in order: `events`, `ids`.
    log: Mutex<Vec<&'static str>>,
}

impl Fake {
    fn events(&self) -> Vec<String> {
        self.stored_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn attempts(&self) -> Vec<String> {
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn insert(&self, lines: &str, token: &str, into: &Mutex<Vec<String>>) -> Result<(), Error> {
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(token.to_owned());
        if self.always_fail.load(Ordering::Relaxed) {
            return Err(Error::Io("down".to_owned()));
        }
        if self
            .transient_failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Error::Io("down".to_owned()));
        }
        if lines.contains(POISON) {
            return Err(Error::InvalidRow("rejected".to_owned()));
        }
        let new = self
            .tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token.to_owned());
        if new {
            into.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(lines.lines().map(str::to_owned));
        }
        Ok(())
    }
}

impl Destination for Fake {
    fn insert_event_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert(lines, token, &self.stored_events)?;
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push("events");
        Ok(())
    }

    fn known_event_ids(&self, keys: &[(u32, &str)]) -> Result<HashSet<(u32, String)>, Error> {
        if self
            .lookups_to_fail
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Error::Io("lookup down".to_owned()));
        }
        if self.lookup_rejected.load(Ordering::Relaxed) {
            return Err(Error::InvalidRow("lookup rejected".to_owned()));
        }
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(keys
            .iter()
            .map(|(site, id)| (*site, (*id).to_owned()))
            .filter(|key| known.contains(key))
            .collect())
    }

    fn insert_event_id_lines(&self, lines: &str, _token: &str) -> Result<(), Error> {
        if self.ids_rejected.load(Ordering::Relaxed) {
            return Err(Error::InvalidRow("ids rejected".to_owned()));
        }
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        for line in lines.lines() {
            let value: serde_json::Value = serde_json::from_str(line).expect("a json line");
            known.insert((
                u32::try_from(value["site_id"].as_u64().expect("a site")).expect("fits"),
                value["event_id"].as_str().expect("an id").to_owned(),
            ));
        }
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push("ids");
        Ok(())
    }

    fn insert_drop_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert(lines, token, &self.stored_drops)
    }

    fn insert_alias_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert(lines, token, &self.stored_aliases)
    }

    fn insert_traits_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        self.insert(lines, token, &self.stored_traits)
    }
}

fn start(spool: &Spool, fake: &Arc<Fake>) -> Batcher {
    Batcher::start(
        spool.clone(),
        Arc::clone(fake) as Arc<dyn Destination>,
        fast(),
    )
    .expect("thread")
}

/// Whether the spool still holds records, read from a fresh handle.
fn pending(dir: &PathBuf) -> usize {
    let spool = open(dir);
    spool.read(usize::MAX, usize::MAX).expect("read").len()
}

// The record format and the sink.

#[test]
fn a_request_becomes_one_record_with_its_event_and_drop_lines() {
    let dir = spool_dir("record");
    let spool = open(&dir);
    let done = enqueue(
        &spool,
        &request(vec![accepted(1, 3), accepted(2, 3)], 4),
        7,
        NOW,
    )
    .unwrap();
    assert_eq!(
        done,
        Enqueued {
            events: 2,
            invalid: 0,
            records: 1
        }
    );
    let records = spool.read(10, 1 << 20).unwrap();
    assert_eq!(records.len(), 1);
    let lines = sink::decode(&records[0].payload).expect("a record");
    assert_eq!(lines.events.lines().count(), 2);
    assert!(
        lines
            .events
            .contains("01890000-0000-7000-8000-000000000001")
    );
    assert_eq!(lines.drops.lines().count(), 1);
    assert!(lines.drops.contains("missing_required_field") && lines.drops.contains("\"count\":4"));
}

#[test]
fn only_drops_still_make_a_record_and_nothing_at_all_makes_none() {
    let dir = spool_dir("drops-only");
    let spool = open(&dir);
    let done = enqueue(&spool, &request(vec![], 2), 7, NOW).unwrap();
    assert_eq!((done.events, done.records), (0, 1));
    let done = enqueue(&spool, &request(vec![], 0), 7, NOW).unwrap();
    assert_eq!((done.events, done.records), (0, 0));
    assert_eq!(spool.read(10, 1 << 20).unwrap().len(), 1);
}

#[test]
fn an_event_that_cannot_be_a_row_is_counted_as_an_invalid_row_drop() {
    let dir = spool_dir("invalid");
    let spool = open(&dir);
    let mut bad = accepted(1, 1);
    bad.schema_version = 70_000;
    let done = enqueue(&spool, &request(vec![bad, accepted(2, 1)], 0), 7, NOW).unwrap();
    assert_eq!((done.events, done.invalid), (1, 1));
    let lines_text = {
        let records = spool.read(10, 1 << 20).unwrap();
        let lines = sink::decode(&records[0].payload).unwrap();
        (lines.events.to_owned(), lines.drops.to_owned())
    };
    assert_eq!(lines_text.0.lines().count(), 1);
    assert!(lines_text.1.contains(sink::INVALID_ROW) && lines_text.1.contains("checkout"));
}

#[test]
fn a_large_request_is_split_by_events_and_the_drops_ride_in_the_first_record() {
    let dir = spool_dir("chunks");
    let spool = open(&dir);
    let events: Vec<_> = (0..150).map(|n| accepted(n, 30_000)).collect();
    let done = enqueue(&spool, &request(events, 5), 7, NOW).unwrap();
    assert_eq!(done.events, 150);
    assert!(done.records >= 2, "{done:?}");
    let records = spool.read(100, usize::MAX).unwrap();
    assert_eq!(records.len(), done.records);
    let decoded: Vec<_> = records
        .iter()
        .map(|r| sink::decode(&r.payload).unwrap())
        .collect();
    assert_eq!(
        decoded
            .iter()
            .map(|l| l.events.lines().count())
            .sum::<usize>(),
        150
    );
    assert!(!decoded[0].drops.is_empty());
    assert!(decoded[1..].iter().all(|l| l.drops.is_empty()));
}

#[test]
fn a_full_spool_is_reported_to_the_caller() {
    let dir = spool_dir("full");
    let mut config = SpoolConfig::new(&dir);
    config.sync = false;
    config.quota_bytes = 2_000;
    config.segment_bytes = 1_000;
    let spool = Spool::open(config).unwrap();
    let mut refused = false;
    for n in 0..50 {
        if let Err(error) = enqueue(&spool, &request(vec![accepted(n, 100)], 0), 7, NOW) {
            assert_eq!(error, dionaea_spool::SpoolError::Full);
            refused = true;
            break;
        }
    }
    assert!(refused);
}

#[test]
fn decode_refuses_anything_this_version_did_not_write() {
    assert!(sink::decode(b"").is_none());
    assert!(sink::decode(&[9, 0, 0, 0, 0]).is_none(), "another version");
    assert!(sink::decode(&[1, 0, 0]).is_none(), "a short header");
    assert!(
        sink::decode(&[1, 9, 0, 0, 0, b'a']).is_none(),
        "a length past the end"
    );
    assert!(sink::decode(&[1, 1, 0, 0, 0, 0xFF]).is_none(), "not UTF-8");
    assert_eq!(
        sink::decode(&[1, 2, 0, 0, 0, b'a', b'\n', b'd']).map(|l| (l.events, l.drops)),
        Some(("a\n", "d")),
        "records of the first version are still read"
    );
    let header = |a: u32, b: u32, c: u32| {
        let mut bytes = vec![2];
        for n in [a, b, c] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes
    };
    let mut record = header(1, 2, 3);
    record.extend_from_slice(b"eaabbbd");
    let lines = sink::decode(&record).expect("a record");
    assert_eq!(
        (lines.events, lines.aliases, lines.traits, lines.drops),
        ("e", "aa", "bbb", "d")
    );
    assert!(
        sink::decode(&header(1, 2, 3)).is_none(),
        "lengths past the end"
    );
    assert!(sink::decode(&[2, 1, 0, 0, 0]).is_none(), "a short header");
    let mut long = header(0, 0, u32::MAX);
    long.push(b'x');
    assert!(sink::decode(&long).is_none(), "a huge length does not wrap");
}

// The batcher.

#[test]
fn what_is_enqueued_reaches_the_destination_and_the_spool_drains() {
    let dir = spool_dir("happy");
    let spool = open(&dir);
    for n in 0..5 {
        enqueue(
            &spool,
            &request(vec![accepted(n, 1), accepted(n + 100, 1)], 1),
            7,
            NOW,
        )
        .unwrap();
    }
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    wait_for("the rows", || batcher.stats().event_rows() == 10);
    wait_for("the drop counts", || batcher.stats().drop_rows() == 5);

    assert_eq!(batcher.stats().last_error(), None);
    batcher.stop();
    assert_eq!(fake.events().len(), 10);
    drop(spool);
    assert_eq!(pending(&dir), 0, "everything was confirmed and committed");
}

#[test]
fn transient_failures_are_retried_under_the_same_token_and_lose_nothing() {
    let dir = spool_dir("transient");
    let spool = open(&dir);
    enqueue(
        &spool,
        &request(vec![accepted(1, 1), accepted(2, 1)], 2),
        7,
        NOW,
    )
    .unwrap();
    let fake = Arc::new(Fake::default());
    fake.transient_failures.store(3, Ordering::SeqCst);
    let batcher = start(&spool, &fake);
    wait_for("the rows", || batcher.stats().event_rows() == 2);
    assert!(batcher.stats().retries() >= 3);
    batcher.stop();
    assert_eq!(fake.events().len(), 2);
    let attempts = fake.attempts();
    let events: Vec<_> = attempts.iter().filter(|t| t.ends_with("-e")).collect();
    assert!(events.len() >= 4, "{attempts:?}");
    assert!(
        events.windows(2).all(|w| w[0] == w[1]),
        "one token for every retry: {attempts:?}"
    );
}

#[test]
fn a_poison_record_is_skipped_and_the_rest_of_its_batch_is_stored() {
    let dir = spool_dir("poison");
    let spool = open(&dir);
    enqueue(&spool, &request(vec![accepted(1, 1)], 0), 7, NOW).unwrap();
    let mut poison = accepted(2, 1);
    poison.strings = vec![("note".to_owned(), POISON.to_owned())];
    enqueue(&spool, &request(vec![poison], 0), 7, NOW).unwrap();
    enqueue(&spool, &request(vec![accepted(3, 1)], 0), 7, NOW).unwrap();
    let fake = Arc::new(Fake::default());
    // Join the three records in one batch.
    let batcher = start(&spool, &fake);
    wait_for("the good rows", || {
        fake.events().len() == 2 && batcher.stats().skipped_records() == 1
    });
    batcher.stop();
    assert!(fake.events().iter().all(|l| !l.contains(POISON)));
    drop(spool);
    assert_eq!(pending(&dir), 0, "the queue was not blocked");
}

#[test]
fn a_record_that_cannot_be_read_is_skipped_and_counted() {
    let dir = spool_dir("garbage");
    let spool = open(&dir);
    spool.append(b"not a record".to_vec()).unwrap();
    enqueue(&spool, &request(vec![accepted(1, 1)], 0), 7, NOW).unwrap();
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    wait_for("the good row", || fake.events().len() == 1);
    assert_eq!(batcher.stats().skipped_records(), 1);
    batcher.stop();
    drop(spool);
    assert_eq!(pending(&dir), 0);
}

#[test]
fn a_restart_reads_unconfirmed_records_again_with_the_same_token() {
    let dir = spool_dir("restart");
    let first_attempts;
    {
        let spool = open(&dir);
        enqueue(&spool, &request(vec![accepted(1, 1)], 1), 7, NOW).unwrap();
        enqueue(&spool, &request(vec![accepted(2, 1)], 1), 7, NOW).unwrap();
        let down = Arc::new(Fake::default());
        down.always_fail.store(true, Ordering::Relaxed);
        let batcher = start(&spool, &down);
        wait_for("a few attempts", || batcher.stats().retries() >= 2);
        batcher.stop();
        assert!(down.events().is_empty());
        first_attempts = down.attempts();
    }
    assert_eq!(
        pending(&dir),
        2,
        "nothing was confirmed, nothing was committed"
    );

    let spool = open(&dir);
    let healthy = Arc::new(Fake::default());
    let batcher = start(&spool, &healthy);
    wait_for("the rows", || healthy.events().len() == 2);
    batcher.stop();
    let second = healthy.attempts();
    assert_eq!(
        first_attempts[0], second[0],
        "the same batch has the same token"
    );
}

#[test]
fn stopping_interrupts_a_long_wait_after_a_failure() {
    let dir = spool_dir("stop");
    let spool = open(&dir);
    enqueue(&spool, &request(vec![accepted(1, 1)], 0), 7, NOW).unwrap();
    let down = Arc::new(Fake::default());
    down.always_fail.store(true, Ordering::Relaxed);
    let config = BatcherConfig {
        backoff_min: Duration::from_secs(60),
        backoff_max: Duration::from_secs(60),
        ..fast()
    };
    let batcher = Batcher::start(
        spool.clone(),
        Arc::clone(&down) as Arc<dyn Destination>,
        config,
    )
    .expect("thread");
    wait_for("the first failure", || batcher.stats().retries() >= 1);
    let started = Instant::now();
    batcher.stop();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_busy_spool_is_drained_in_large_inserts_not_one_per_record() {
    let dir = spool_dir("batching");
    let spool = open(&dir);
    for n in 0..200 {
        enqueue(&spool, &request(vec![accepted(n, 1)], 0), 7, NOW).unwrap();
    }
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    wait_for("the rows", || batcher.stats().event_rows() == 200);
    assert!(
        batcher.stats().batches() <= 5,
        "{} inserts for 200 records",
        batcher.stats().batches()
    );
    batcher.stop();
}

#[test]
fn a_second_batcher_on_the_same_spool_reads_what_the_first_never_confirmed() {
    let dir = spool_dir("same-spool");
    let spool = open(&dir);
    enqueue(
        &spool,
        &request(vec![accepted(1, 1), accepted(2, 1)], 0),
        7,
        NOW,
    )
    .unwrap();
    let down = Arc::new(Fake::default());
    down.always_fail.store(true, Ordering::Relaxed);
    let first = start(&spool, &down);
    wait_for("a failed attempt", || first.stats().retries() >= 1);
    first.stop();

    let healthy = Arc::new(Fake::default());
    let second = start(&spool, &healthy);
    wait_for("the rows", || healthy.events().len() == 2);
    second.stop();
}

#[test]
fn a_rejected_drop_line_never_repeats_the_events_stored_with_it() {
    let dir = spool_dir("drops-poison");
    let spool = open(&dir);
    // Two requests whose events are fine; the second one's drop lines are
    // rejected for good. They are joined in one batch.
    enqueue(&spool, &request(vec![accepted(1, 1)], 1), 7, NOW).unwrap();
    let mut second = request(vec![accepted(2, 1)], 1);
    second.drops[0].event_name = POISON.to_owned();
    enqueue(&spool, &second, 7, NOW).unwrap();
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    wait_for("the events and the isolated drops", || {
        fake.events().len() == 2 && batcher.stats().skipped_records() == 1
    });
    // Give a wrong implementation the time to insert the events again.
    std::thread::sleep(Duration::from_millis(200));
    batcher.stop();
    assert_eq!(fake.events().len(), 2, "each event was stored once");
    assert_eq!(
        fake.stored_drops.lock().unwrap().len(),
        1,
        "the good drop line was stored"
    );
    drop(spool);
    assert_eq!(pending(&dir), 0);
}

#[test]
fn the_largest_valid_request_is_always_one_record() {
    // A body is at most 256 KiB. A hundred events with values made only of
    // characters that JSON escapes are the worst case for the lines.
    let dir = spool_dir("one-record");
    let spool = open(&dir);
    let events: Vec<_> = (0..100)
        .map(|n| {
            let mut event = accepted(n, 0);
            event.strings = vec![("note".to_owned(), "\"".repeat(2_500))];
            event
        })
        .collect();
    let done = enqueue(&spool, &request(events, 3), 7, NOW).unwrap();
    assert_eq!((done.events, done.records), (100, 1));
}

#[test]
fn drop_rows_become_records_with_only_drop_lines_and_reach_the_destination() {
    let dir = spool_dir("drop-rows");
    let spool = open(&dir);
    let rows: Vec<dionaea_store::DropRow> = (0..7)
        .map(|n| dionaea_store::DropRow {
            site_id: 7,
            event_name: format!("e{n}"),
            reason: "bot".to_owned(),
            at_ms: NOW,
            count: 2,
        })
        .collect();
    assert_eq!(enqueue_drop_rows(&spool, rows).unwrap(), 7);
    let records = spool.read(10, usize::MAX).unwrap();
    assert_eq!(records.len(), 1);
    let lines = sink::decode(&records[0].payload).unwrap();
    assert!(lines.events.is_empty());
    assert_eq!(lines.drops.lines().count(), 7);

    spool.rewind();
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    wait_for("the drop rows", || batcher.stats().drop_rows() == 7);
    assert_eq!(batcher.stats().event_rows(), 0);
    batcher.stop();
    assert_eq!(fake.stored_drops.lock().unwrap().len(), 7);
}

#[test]
fn drop_rows_that_cannot_be_written_come_back_to_the_caller() {
    let dir = spool_dir("drop-rows-full");
    let mut config = SpoolConfig::new(&dir);
    config.sync = false;
    config.quota_bytes = 600;
    config.segment_bytes = 300;
    let spool = Spool::open(config).unwrap();
    let rows: Vec<dionaea_store::DropRow> = (0..40_000)
        .map(|n| dionaea_store::DropRow {
            site_id: 7,
            event_name: format!("event_{n}"),
            reason: "bot".to_owned(),
            at_ms: NOW,
            count: 1,
        })
        .collect();
    let (error, rest) = enqueue_drop_rows(&spool, rows).unwrap_err();
    assert!(
        matches!(
            error,
            dionaea_spool::SpoolError::Full | dionaea_spool::SpoolError::TooLarge
        ),
        "{error:?}"
    );
    assert!(!rest.is_empty(), "nothing is lost");
}

// Events another node already stored.

fn same_event(n: u32) -> Processed {
    request(vec![accepted(n, 1)], 0)
}

fn keys_of(fake: &Fake) -> Vec<(u32, String)> {
    let mut keys: Vec<_> = fake
        .known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn an_event_another_node_already_stored_is_left_out() {
    let (dir_a, dir_b) = (spool_dir("node-a"), spool_dir("node-b"));
    let (spool_a, spool_b) = (open(&dir_a), open(&dir_b));
    // The same event reached two nodes (an SDK retry through a balancer).
    enqueue(&spool_a, &same_event(1), 7, NOW).unwrap();
    enqueue(&spool_b, &same_event(1), 7, NOW).unwrap();
    enqueue(&spool_b, &same_event(2), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());

    let first = start(&spool_a, &store);
    wait_for("node a", || first.stats().event_rows() == 1);
    first.stop();
    assert_eq!(keys_of(&store).len(), 1, "its id is recorded");

    let second = start(&spool_b, &store);
    wait_for("node b", || second.stats().duplicates() == 1);
    wait_for("node b to drain", || second.stats().event_rows() == 1);
    second.stop();
    assert_eq!(store.events().len(), 2, "event 1 once, event 2 once");
    assert_eq!(keys_of(&store).len(), 2);
    drop((spool_a, spool_b));
    assert_eq!(
        pending(&dir_b),
        0,
        "the duplicate was confirmed, not left behind"
    );
}

#[test]
fn the_same_event_twice_in_one_batch_is_stored_once() {
    let dir = spool_dir("in-batch");
    let spool = open(&dir);
    enqueue(&spool, &same_event(5), 7, NOW).unwrap();
    enqueue(&spool, &same_event(5), 7, NOW).unwrap();
    enqueue(&spool, &same_event(6), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());
    let batcher = start(&spool, &store);
    wait_for("the rows", || batcher.stats().event_rows() >= 2);
    let stats = batcher.stats_handle();
    batcher.stop();
    assert_eq!(store.events().len(), 2);
    assert_eq!(stats.duplicates(), 1);
}

#[test]
fn ids_are_recorded_only_after_the_events_are_stored() {
    let dir = spool_dir("order");
    let spool = open(&dir);
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());
    store.transient_failures.store(3, Ordering::SeqCst);
    let batcher = start(&spool, &store);
    wait_for("the rows", || batcher.stats().event_rows() == 1);
    wait_for("the ids", || keys_of(&store).len() == 1);
    batcher.stop();
    let log = store.log.lock().unwrap().clone();
    assert_eq!(
        log,
        ["events", "ids"],
        "no id was recorded for the failed attempts"
    );
}

#[test]
fn a_failed_lookup_is_retried_and_never_answered_by_inserting_blindly() {
    let dir = spool_dir("lookup-retry");
    let spool = open(&dir);
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());
    // The event is already stored elsewhere, but the lookup fails three times.
    store
        .known
        .lock()
        .unwrap()
        .insert((7, "01890000-0000-7000-8000-000000000001".to_owned()));
    store.lookups_to_fail.store(3, Ordering::SeqCst);
    let batcher = start(&spool, &store);
    wait_for("the duplicate", || batcher.stats().duplicates() == 1);
    let stats = batcher.stats_handle();
    batcher.stop();
    assert!(
        store.events().is_empty(),
        "nothing was inserted while the lookup failed"
    );
    assert!(stats.retries() >= 3);
}

#[test]
fn a_lookup_the_store_rejects_for_good_lets_the_batch_through() {
    let dir = spool_dir("lookup-rejected");
    let spool = open(&dir);
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());
    store.lookup_rejected.store(true, Ordering::Relaxed);
    let batcher = start(&spool, &store);
    wait_for("the row", || batcher.stats().event_rows() == 1);
    batcher.stop();
    assert_eq!(
        store.events().len(),
        1,
        "an event stored twice beats one lost"
    );
}

#[test]
fn ids_that_cannot_be_recorded_for_good_do_not_lose_or_block_events() {
    let dir = spool_dir("ids-rejected");
    let spool = open(&dir);
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    enqueue(&spool, &same_event(2), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());
    store.ids_rejected.store(true, Ordering::Relaxed);
    let batcher = start(&spool, &store);
    wait_for("the rows", || batcher.stats().event_rows() == 2);
    batcher.stop();
    assert_eq!(store.events().len(), 2);
    drop(spool);
    assert_eq!(pending(&dir), 0);
}

#[test]
fn with_the_shared_check_off_every_event_is_inserted() {
    let dir = spool_dir("shared-off");
    let spool = open(&dir);
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    let store = Arc::new(Fake::default());
    store
        .known
        .lock()
        .unwrap()
        .insert((7, "01890000-0000-7000-8000-000000000001".to_owned()));
    let config = BatcherConfig {
        shared_dedup: false,
        ..fast()
    };
    let batcher = Batcher::start(
        spool.clone(),
        Arc::clone(&store) as Arc<dyn Destination>,
        config,
    )
    .expect("thread");
    wait_for("the row", || batcher.stats().event_rows() == 1);
    let stats = batcher.stats_handle();
    batcher.stop();
    assert_eq!(stats.duplicates(), 0);
    assert_eq!(
        store.log.lock().unwrap().as_slice(),
        ["events"],
        "no ids recorded either"
    );
}

#[test]
fn a_site_is_part_of_the_key() {
    let dir = spool_dir("sites");
    let spool = open(&dir);
    let mut other_site = accepted(1, 1);
    other_site.site_id = 8;
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    enqueue(&spool, &request(vec![other_site], 0), 8, NOW).unwrap();
    let store = Arc::new(Fake::default());
    let batcher = start(&spool, &store);
    wait_for("the rows", || batcher.stats().event_rows() == 2);
    batcher.stop();
    assert_eq!(
        store.events().len(),
        2,
        "the same id under two sites is two events"
    );
}

#[test]
fn the_cross_node_check_can_be_turned_off_while_the_batcher_runs() {
    let dir = spool_dir("toggle");
    let spool = open(&dir);
    let store = Arc::new(Fake::default());
    store
        .known
        .lock()
        .unwrap()
        .insert((7, "01890000-0000-7000-8000-000000000001".to_owned()));
    let batcher = start(&spool, &store);

    // With the check on, an event another node stored is left out.
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    wait_for("the duplicate", || batcher.stats().duplicates() == 1);

    // Turned off, the same event goes in.
    batcher.reconfigure(BatcherConfig {
        shared_dedup: false,
        ..fast()
    });
    enqueue(&spool, &same_event(1), 7, NOW).unwrap();
    wait_for("the row", || batcher.stats().event_rows() == 1);
    assert_eq!(batcher.stats().duplicates(), 1, "no longer counted");
    batcher.stop();
}

#[test]
fn the_size_of_a_batch_follows_the_configuration_in_force() {
    let dir = spool_dir("batch-size");
    let spool = open(&dir);
    for n in 0..30 {
        enqueue(&spool, &same_event(n), 7, NOW).unwrap();
    }
    let store = Arc::new(Fake::default());
    // One record per insert.
    let config = BatcherConfig {
        max_records: 1,
        ..fast()
    };
    let batcher = Batcher::start(
        spool.clone(),
        Arc::clone(&store) as Arc<dyn Destination>,
        config,
    )
    .unwrap();
    wait_for("the rows", || batcher.stats().event_rows() == 30);
    assert!(
        batcher.stats().batches() >= 30,
        "{} inserts",
        batcher.stats().batches()
    );
    batcher.stop();
}

// Identity.

fn link(anon: &str, user: &str) -> AliasRow {
    AliasRow {
        site_id: 7,
        anon_id: anon.to_owned(),
        user_id: user.to_owned(),
        linked_ms: NOW,
    }
}

fn traits_of(user: &str, plan: &str) -> TraitsRow {
    TraitsRow {
        site_id: 7,
        user_id: user.to_owned(),
        traits_str: vec![("plan".to_owned(), plan.to_owned())],
        traits_num: vec![("seats".to_owned(), 2.0)],
        updated_ms: NOW,
    }
}

#[test]
fn an_identify_request_is_one_record_with_its_link_traits_and_drops() {
    let dir = spool_dir("identity-record");
    let spool = open(&dir);
    let drops = [dionaea_store::DropRow {
        site_id: 7,
        event_name: "(identify)".to_owned(),
        reason: "invalid_field".to_owned(),
        at_ms: NOW,
        count: 2,
    }];
    sink::enqueue_identity(
        &spool,
        &link("a1", "u1"),
        Some(&traits_of("u1", "pro")),
        &drops,
    )
    .unwrap();
    sink::enqueue_identity(&spool, &link("a2", "u2"), None, &[]).unwrap();
    let records = spool.read(10, 1 << 20).unwrap();
    assert_eq!(records.len(), 2);
    let first = sink::decode(&records[0].payload).expect("a record");
    assert_eq!(first.events, "");
    assert!(first.aliases.contains("\"anon_id\":\"a1\"") && first.aliases.contains("u1"));
    assert!(first.traits.contains("\"plan\":\"pro\""));
    assert_eq!(first.drops.lines().count(), 1);
    let second = sink::decode(&records[1].payload).expect("a record");
    assert_eq!(
        (second.traits, second.drops),
        ("", ""),
        "no traits, no drops"
    );
    assert_eq!(second.aliases.lines().count(), 1);
}

#[test]
fn links_and_traits_reach_the_destination_each_under_its_own_token() {
    let dir = spool_dir("identity-deliver");
    let spool = open(&dir);
    enqueue(&spool, &request(vec![accepted(1, 1)], 0), 7, NOW).unwrap();
    sink::enqueue_identity(
        &spool,
        &link("a1", "u1"),
        Some(&traits_of("u1", "pro")),
        &[],
    )
    .unwrap();
    sink::enqueue_identity(&spool, &link("a2", "u1"), None, &[]).unwrap();
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    // Two links and one set of traits.
    wait_for("the identity rows", || batcher.stats().identity_rows() == 3);
    wait_for("the event", || batcher.stats().event_rows() == 1);
    batcher.stop();
    assert_eq!(fake.stored_aliases.lock().unwrap().len(), 2);
    assert_eq!(fake.stored_traits.lock().unwrap().len(), 1);
    let attempts = fake.attempts();
    for suffix in ["-e", "-a", "-t"] {
        assert!(
            attempts.iter().any(|t| t.ends_with(suffix)),
            "a token for {suffix}: {attempts:?}"
        );
    }
    drop(spool);
    assert_eq!(pending(&dir), 0);
}

#[test]
fn an_outage_repeats_a_link_under_one_token_and_stores_it_once() {
    let dir = spool_dir("identity-retry");
    let spool = open(&dir);
    sink::enqueue_identity(
        &spool,
        &link("a1", "u1"),
        Some(&traits_of("u1", "pro")),
        &[],
    )
    .unwrap();
    let fake = Arc::new(Fake::default());
    fake.transient_failures.store(4, Ordering::SeqCst);
    let batcher = start(&spool, &fake);
    wait_for("the link", || batcher.stats().identity_rows() == 2);
    batcher.stop();
    assert_eq!(fake.stored_aliases.lock().unwrap().len(), 1);
    assert_eq!(fake.stored_traits.lock().unwrap().len(), 1);
    let attempts = fake.attempts();
    let aliases: Vec<_> = attempts.iter().filter(|t| t.ends_with("-a")).collect();
    assert!(aliases.windows(2).all(|w| w[0] == w[1]), "{attempts:?}");
}

#[test]
fn rejected_traits_never_repeat_the_link_stored_with_them() {
    let dir = spool_dir("identity-poison");
    let spool = open(&dir);
    sink::enqueue_identity(
        &spool,
        &link("a1", "u1"),
        Some(&traits_of("u1", POISON)),
        &[],
    )
    .unwrap();
    let fake = Arc::new(Fake::default());
    let batcher = start(&spool, &fake);
    wait_for("the link", || batcher.stats().identity_rows() == 1);
    wait_for("the skip", || batcher.stats().skipped_records() == 1);
    batcher.stop();
    assert_eq!(fake.stored_aliases.lock().unwrap().len(), 1);
    assert!(fake.stored_traits.lock().unwrap().is_empty());
    let attempts = fake.attempts();
    assert_eq!(
        attempts.iter().filter(|t| t.ends_with("-a")).count(),
        1,
        "the link is inserted once: {attempts:?}"
    );
    drop(spool);
    assert_eq!(pending(&dir), 0, "the poison does not block the spool");
}

// Erased users and devices.

/// Tombstones for user `u_erased` of site 7 and its devices `gone1` and
/// `gone2`, made the way the server makes them.
fn erased_set() -> Arc<dionaea_meta::Tombstones> {
    use dionaea_meta::MetaStore;
    let meta = dionaea_meta::SqliteMeta::open_in_memory().expect("meta");
    let organization = meta.create_organization("o").expect("organization");
    // Sites 1 to 7, so that the erased user belongs to site 7.
    for n in 1..=7 {
        meta.create_site(organization.id, &format!("s{n}"))
            .expect("site");
    }
    let request = meta
        .request_erasure(7, "u_erased", "test")
        .expect("request");
    meta.add_erasure_anons(request.id, &["gone1".to_owned(), "gone2".to_owned()])
        .expect("anons");
    let all = dionaea_meta::Tombstones::default();
    all.load_from(&meta).expect("tombstones");
    Arc::new(all)
}

fn source(tombstones: Arc<dionaea_meta::Tombstones>) -> TombstoneSource {
    Arc::new(move || Arc::clone(&tombstones))
}

fn anon_event(n: u32, anon: &str) -> Accepted {
    Accepted {
        anon_id: anon.to_owned(),
        ..accepted(n, 1)
    }
}

#[test]
fn rows_made_durable_before_the_erasure_are_left_out_when_the_batch_is_inserted() {
    let dir = spool_dir("erased-batch");
    let spool = open(&dir);
    enqueue(
        &spool,
        &request(
            vec![
                anon_event(1, "gone1"),
                anon_event(2, "kept"),
                anon_event(3, "gone2"),
                anon_event(4, "kept"),
            ],
            0,
        ),
        7,
        NOW,
    )
    .unwrap();
    // The same devices, identified; and the erased user on a device not yet
    // known as theirs; and someone else.
    let alias = |anon: &str, user: &str| AliasRow {
        site_id: 7,
        anon_id: anon.to_owned(),
        user_id: user.to_owned(),
        linked_ms: NOW,
    };
    sink::enqueue_identity(&spool, &alias("gone1", "u_other"), None, &[]).unwrap();
    sink::enqueue_identity(
        &spool,
        &alias("fresh", "u_erased"),
        Some(&traits_of("u_erased", "pro")),
        &[],
    )
    .unwrap();
    sink::enqueue_identity(
        &spool,
        &alias("fine", "u_fine"),
        Some(&traits_of("u_fine", "free")),
        &[],
    )
    .unwrap();

    let fake = Arc::new(Fake::default());
    let batcher = Batcher::start_with(
        spool.clone(),
        Arc::clone(&fake) as Arc<dyn Destination>,
        fast(),
        source(erased_set()),
    )
    .expect("thread");
    wait_for("the rows", || {
        batcher.stats().event_rows() == 2 && batcher.stats().identity_rows() == 2
    });
    batcher.stop();

    let events = fake.events();
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|line| line.contains("\"anon_id\":\"kept\""))
    );
    let aliases = fake.stored_aliases.lock().unwrap().clone();
    assert_eq!(aliases.len(), 1, "{aliases:?}");
    assert!(aliases[0].contains("u_fine"));
    let traits = fake.stored_traits.lock().unwrap().clone();
    assert_eq!(traits.len(), 1, "{traits:?}");
    assert!(traits[0].contains("free") && !traits.iter().any(|t| t.contains("u_erased")));
    drop(spool);
    assert_eq!(pending(&dir), 0, "everything was committed, erased or not");
}

#[test]
fn the_erased_count_is_kept_and_a_batch_of_only_erased_rows_drains() {
    let dir = spool_dir("erased-only");
    let spool = open(&dir);
    enqueue(
        &spool,
        &request(vec![anon_event(1, "gone1"), anon_event(2, "gone2")], 0),
        7,
        NOW,
    )
    .unwrap();
    let fake = Arc::new(Fake::default());
    let batcher = Batcher::start_with(
        spool.clone(),
        Arc::clone(&fake) as Arc<dyn Destination>,
        fast(),
        source(erased_set()),
    )
    .expect("thread");
    wait_for("the erased rows", || batcher.stats().erased_rows() == 2);
    // A row that is kept, after them: once it is stored the erased batch has
    // been committed too.
    enqueue(&spool, &request(vec![anon_event(3, "kept")], 0), 7, NOW).unwrap();
    wait_for("the kept row", || batcher.stats().event_rows() == 1);
    assert_eq!(batcher.stats().erased_rows(), 2);
    batcher.stop();
    assert_eq!(fake.events().len(), 1);
    drop(spool);
    assert_eq!(pending(&dir), 0, "the erased rows were committed, not kept");
}

#[test]
fn another_site_with_the_same_names_is_not_filtered() {
    let dir = spool_dir("erased-site");
    let spool = open(&dir);
    let mut other_site = anon_event(1, "gone1");
    other_site.site_id = 3;
    enqueue(&spool, &request(vec![other_site], 0), 3, NOW).unwrap();
    let fake = Arc::new(Fake::default());
    let batcher = Batcher::start_with(
        spool.clone(),
        Arc::clone(&fake) as Arc<dyn Destination>,
        fast(),
        source(erased_set()),
    )
    .expect("thread");
    wait_for("the row", || batcher.stats().event_rows() == 1);
    assert_eq!(batcher.stats().erased_rows(), 0);
    batcher.stop();
}

#[test]
fn a_tombstone_that_arrives_later_applies_to_the_next_batch() {
    use std::sync::RwLock;
    let dir = spool_dir("erased-late");
    let spool = open(&dir);
    let current: Arc<RwLock<Arc<dionaea_meta::Tombstones>>> = Arc::default();
    let reader = Arc::clone(&current);
    let late: TombstoneSource = Arc::new(move || Arc::clone(&reader.read().unwrap()));
    let fake = Arc::new(Fake::default());
    let batcher = Batcher::start_with(
        spool.clone(),
        Arc::clone(&fake) as Arc<dyn Destination>,
        fast(),
        late,
    )
    .expect("thread");

    enqueue(&spool, &request(vec![anon_event(1, "gone1")], 0), 7, NOW).unwrap();
    wait_for("the first row", || batcher.stats().event_rows() == 1);

    *current.write().unwrap() = erased_set();
    enqueue(&spool, &request(vec![anon_event(2, "gone1")], 0), 7, NOW).unwrap();
    wait_for("the second to be left out", || {
        batcher.stats().erased_rows() == 1
    });
    batcher.stop();
    assert_eq!(fake.events().len(), 1, "only what came before");
}
