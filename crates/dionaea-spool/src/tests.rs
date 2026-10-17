use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use super::*;

/// A fresh directory, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "dionaea-spool-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }

    fn config(&self) -> Config {
        let mut config = Config::new(&self.0);
        config.sync = false;
        config
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn payloads(records: &[Record]) -> Vec<Vec<u8>> {
    records.iter().map(|r| r.payload.clone()).collect()
}

#[test]
fn records_come_back_in_order_with_their_positions() {
    let dir = TestDir::new();
    let spool = Spool::open(dir.config()).unwrap();
    let first = spool.append(b"one").unwrap();
    let second = spool.append(b"two").unwrap();
    assert!(first < second);
    assert_eq!(
        first,
        Position {
            segment: 1,
            offset: HEADER_LEN
        }
    );
    assert_eq!(second.offset, HEADER_LEN + RECORD_OVERHEAD + 3);

    let records = spool.read(10, usize::MAX).unwrap();
    assert_eq!(payloads(&records), vec![b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(records[0].id, first);
    assert_eq!(records[0].next, second);
    assert!(spool.read(10, usize::MAX).unwrap().is_empty());
}

#[test]
fn read_respects_the_record_and_byte_limits() {
    let dir = TestDir::new();
    let spool = Spool::open(dir.config()).unwrap();
    for i in 0..10u8 {
        spool.append([i; 10]).unwrap();
    }
    assert_eq!(spool.read(3, usize::MAX).unwrap().len(), 3);
    // 25 bytes of payload allow three 10-byte records: the limit is checked
    // before each record, so the third is still read.
    assert_eq!(spool.read(100, 25).unwrap().len(), 3);
    // At least one record comes back even when it alone exceeds the limit.
    assert_eq!(spool.read(100, 1).unwrap().len(), 1);
    assert_eq!(spool.read(100, usize::MAX).unwrap().len(), 3);
}

#[test]
fn bad_payloads_are_refused() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.max_record_bytes = 8;
    let spool = Spool::open(config).unwrap();
    assert_eq!(spool.append(b""), Err(SpoolError::Empty));
    assert_eq!(spool.append([0u8; 9]), Err(SpoolError::TooLarge));
    assert!(spool.append([0u8; 8]).is_ok());
}

#[test]
fn a_full_quota_refuses_appends_and_commit_frees_it() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.segment_bytes = 64;
    config.quota_bytes = 200;
    let spool = Spool::open(config).unwrap();
    let mut accepted = 0;
    while spool.append([1u8; 20]).is_ok() {
        accepted += 1;
        assert!(accepted < 100, "the quota never bit");
    }
    assert!(accepted >= 3);
    assert_eq!(spool.append([1u8; 20]), Err(SpoolError::Full));
    assert!(spool.stats().bytes_used <= 200);

    let records = spool.read(100, usize::MAX).unwrap();
    assert_eq!(records.len(), accepted);
    spool.commit(records.last().unwrap().next).unwrap();
    assert!(
        spool.append([1u8; 20]).is_ok(),
        "committed segments free the quota"
    );
}

#[test]
fn records_never_span_segments_and_every_one_is_read() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.segment_bytes = 100;
    let spool = Spool::open(config).unwrap();
    for i in 0..50u8 {
        spool.append([i; 30]).unwrap();
    }
    assert!(spool.stats().segments > 5);
    let records = spool.read(1000, usize::MAX).unwrap();
    assert_eq!(records.len(), 50);
    for (i, record) in records.iter().enumerate() {
        assert_eq!(record.payload, vec![i as u8; 30]);
    }
    assert!(records.windows(2).all(|w| w[0].id < w[1].id));
}

#[test]
fn commit_deletes_consumed_segments_and_survives_a_restart() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.segment_bytes = 100;
    {
        let spool = Spool::open(config.clone()).unwrap();
        for i in 0..20u8 {
            spool.append([i; 30]).unwrap();
        }
        let records = spool.read(8, usize::MAX).unwrap();
        assert_eq!(records.len(), 8);
        spool.commit(records[7].next).unwrap();
    }
    let spool = Spool::open(config).unwrap();
    let rest = spool.read(1000, usize::MAX).unwrap();
    assert_eq!(rest.len(), 12, "only what was committed is gone");
    assert_eq!(rest[0].payload, vec![8u8; 30]);
    assert!(
        first_sequence(&dir.0).unwrap().unwrap() > 1,
        "old segments were deleted"
    );
}

#[test]
fn uncommitted_records_are_replayed_after_a_restart_and_after_a_rewind() {
    let dir = TestDir::new();
    let config = dir.config();
    {
        let spool = Spool::open(config.clone()).unwrap();
        spool.append(b"a").unwrap();
        spool.append(b"b").unwrap();
        assert_eq!(spool.read(10, usize::MAX).unwrap().len(), 2);
        // Never committed.
    }
    let spool = Spool::open(config).unwrap();
    assert_eq!(spool.read(10, usize::MAX).unwrap().len(), 2);
    spool.rewind();
    let again = spool.read(10, usize::MAX).unwrap();
    assert_eq!(payloads(&again), vec![b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn commit_rejects_positions_that_were_not_read() {
    let dir = TestDir::new();
    let spool = Spool::open(dir.config()).unwrap();
    spool.append(b"a").unwrap();
    spool.append(b"b").unwrap();
    let records = spool.read(1, usize::MAX).unwrap();
    let ahead = Position {
        segment: 1,
        offset: records[0].next.offset + 100,
    };
    assert_eq!(spool.commit(ahead), Err(SpoolError::BadCommit));
    spool.commit(records[0].next).unwrap();
    assert_eq!(
        spool.commit(Position {
            segment: 1,
            offset: HEADER_LEN
        }),
        Err(SpoolError::BadCommit)
    );
}

#[test]
fn a_torn_tail_is_cut_off_and_counted() {
    let dir = TestDir::new();
    let config = dir.config();
    {
        let spool = Spool::open(config.clone()).unwrap();
        spool.append(b"keep one").unwrap();
        spool.append(b"keep two").unwrap();
    }
    // A record cut in half, as a crash would leave it.
    let path = segment_path(&dir.0, 1);
    let mut torn = Vec::new();
    format::encode_record(&mut torn, b"this one is torn in half");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&torn[..torn.len() / 2]).unwrap();
    drop(file);

    let spool = Spool::open(config).unwrap();
    assert_eq!(spool.stats().truncated_on_open, (torn.len() / 2) as u64);
    let records = spool.read(10, usize::MAX).unwrap();
    assert_eq!(
        payloads(&records),
        vec![b"keep one".to_vec(), b"keep two".to_vec()]
    );
    // New appends land after the good records.
    spool.append(b"after").unwrap();
    assert_eq!(spool.read(10, usize::MAX).unwrap()[0].payload, b"after");
}

#[test]
fn a_damaged_record_ends_its_segment_but_not_the_ones_after() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.segment_bytes = 120;
    {
        let spool = Spool::open(config.clone()).unwrap();
        for i in 0..12u8 {
            spool.append([i; 30]).unwrap();
        }
    }
    // Flip a bit inside the second record of the first segment.
    let path = segment_path(&dir.0, 1);
    let mut bytes = fs::read(&path).unwrap();
    let at = (HEADER_LEN + RECORD_OVERHEAD + 30 + RECORD_OVERHEAD + 5) as usize;
    bytes[at] ^= 0x40;
    fs::write(&path, bytes).unwrap();

    let spool = Spool::open(config).unwrap();
    assert!(spool.stats().truncated_on_open > 0);
    let records = spool.read(1000, usize::MAX).unwrap();
    assert!(
        records.len() < 12 && records.len() >= 6,
        "{}",
        records.len()
    );
    assert_eq!(
        records[0].payload,
        vec![0u8; 30],
        "the good record before the damage survives"
    );
}

#[test]
fn a_header_only_or_headerless_segment_is_handled() {
    let dir = TestDir::new();
    let config = dir.config();
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(segment_path(&dir.0, 1), b"junk").unwrap();
    let spool = Spool::open(config).unwrap();
    assert_eq!(spool.stats().truncated_on_open, 4);
    spool.append(b"works").unwrap();
    assert_eq!(spool.read(10, usize::MAX).unwrap()[0].payload, b"works");
}

#[test]
fn a_second_open_of_the_same_directory_is_refused() {
    let dir = TestDir::new();
    let first = Spool::open(dir.config()).unwrap();
    assert_eq!(Spool::open(dir.config()).unwrap_err(), SpoolError::Locked);
    drop(first);
    assert!(
        Spool::open(dir.config()).is_ok(),
        "the lock is released on drop"
    );
}

#[test]
fn concurrent_appends_are_all_durable_and_share_syncs() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.sync = true;
    let spool = Spool::open(config).unwrap();
    let threads = 16;
    let per_thread = 40;
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let spool = spool.clone();
            thread::spawn(move || {
                for i in 0..per_thread {
                    let payload = format!("{t}:{i}");
                    spool.append(payload.as_bytes()).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let stats = spool.stats();
    assert_eq!(stats.appended_records, (threads * per_thread) as u64);
    assert!(
        stats.syncs < stats.appended_records,
        "{} syncs for {} records",
        stats.syncs,
        stats.appended_records
    );

    let records = spool.read(10_000, usize::MAX).unwrap();
    assert_eq!(records.len(), threads * per_thread);
    // Per thread, the order of acknowledgement is the order on disk.
    for t in 0..threads {
        let own: Vec<usize> = records
            .iter()
            .filter_map(|r| {
                let text = String::from_utf8(r.payload.clone()).unwrap();
                let (thread, i) = text.split_once(':').unwrap();
                (thread == t.to_string()).then(|| i.parse().unwrap())
            })
            .collect();
        assert_eq!(own, (0..per_thread).collect::<Vec<_>>());
    }
}

#[test]
fn the_pending_limit_answers_busy_instead_of_growing() {
    let dir = TestDir::new();
    let mut config = dir.config();
    config.max_pending_bytes = 100;
    let spool = Spool::open(config).unwrap();
    // One record larger than the pending limit can never be queued.
    assert_eq!(spool.append([0u8; 200]), Err(SpoolError::Busy));
    assert!(spool.append([0u8; 50]).is_ok());
}

#[test]
fn appends_after_close_are_refused() {
    let dir = TestDir::new();
    let spool = Spool::open(dir.config()).unwrap();
    let clone = spool.clone();
    lock(&spool.core.shared).closing = true;
    assert_eq!(clone.append(b"late"), Err(SpoolError::Closed));
}

#[test]
fn a_marker_survives_and_a_damaged_one_restarts_from_the_oldest_segment() {
    let dir = TestDir::new();
    let config = dir.config();
    {
        let spool = Spool::open(config.clone()).unwrap();
        spool.append(b"a").unwrap();
        spool.append(b"b").unwrap();
        let records = spool.read(1, usize::MAX).unwrap();
        spool.commit(records[0].next).unwrap();
    }
    assert_eq!(
        decode_marker(&fs::read(dir.0.join(MARKER)).unwrap())
            .unwrap()
            .segment,
        1
    );
    fs::write(dir.0.join(MARKER), b"garbage").unwrap();
    let spool = Spool::open(config).unwrap();
    // The record already committed is delivered again: at-least-once.
    assert_eq!(spool.read(10, usize::MAX).unwrap().len(), 2);
}

#[test]
fn the_marker_round_trips_and_rejects_damage() {
    let position = Position {
        segment: 9,
        offset: 12345,
    };
    let bytes = encode_marker(position);
    assert_eq!(decode_marker(&bytes), Some(position));
    for index in 0..bytes.len() {
        let mut damaged = bytes;
        damaged[index] ^= 0x01;
        assert_eq!(decode_marker(&damaged), None, "byte {index}");
    }
}

fn outcome_within(spool: &Spool, payload: &'static [u8]) -> Result<Position, SpoolError> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let spool = spool.clone();
    let worker = thread::spawn(move || {
        let _ = sender.send(spool.append(payload));
    });
    let outcome = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the append returned instead of hanging");
    // The thread still holds its clone of the spool, and with it the lock on
    // the directory, until it ends; a test that reopens the directory next
    // must not race with that.
    worker.join().expect("the appending thread ends");
    outcome
}

#[test]
fn a_failed_write_fails_that_append_and_every_later_one_and_is_reported() {
    let dir = TestDir::new();
    let spool = Spool::open(dir.config()).unwrap();
    assert!(spool.append(b"fine").is_ok());
    assert_eq!(spool.stats().failed, None);

    spool.core.fault.store(1, Ordering::Relaxed);
    let first = outcome_within(&spool, b"doomed");
    assert!(
        matches!(&first, Err(SpoolError::Io(m)) if m.contains("injected failure")),
        "{first:?}"
    );
    assert!(matches!(
        outcome_within(&spool, b"later"),
        Err(SpoolError::Io(_))
    ));
    assert!(
        spool
            .stats()
            .failed
            .is_some_and(|m| m.contains("injected failure"))
    );
    // What was durable before the failure is still readable.
    assert_eq!(
        payloads(&spool.read(10, usize::MAX).unwrap()),
        vec![b"fine".to_vec()]
    );
}

#[test]
fn a_panic_in_the_writer_does_not_hang_the_appenders() {
    let dir = TestDir::new();
    let spool = Spool::open(dir.config()).unwrap();
    spool.core.fault.store(2, Ordering::Relaxed);
    let first = outcome_within(&spool, b"doomed");
    assert!(
        matches!(&first, Err(SpoolError::Io(m)) if m.contains("panicked")),
        "{first:?}"
    );
    assert!(matches!(
        outcome_within(&spool, b"later"),
        Err(SpoolError::Io(_))
    ));
    assert!(spool.stats().failed.is_some());
}

#[test]
fn reopening_after_a_failure_recovers_what_was_durable() {
    let dir = TestDir::new();
    let config = dir.config();
    {
        let spool = Spool::open(config.clone()).unwrap();
        spool.append(b"durable").unwrap();
        spool.core.fault.store(1, Ordering::Relaxed);
        assert!(outcome_within(&spool, b"lost").is_err());
    }
    let spool = Spool::open(config).unwrap();
    assert_eq!(spool.stats().failed, None);
    assert_eq!(
        payloads(&spool.read(10, usize::MAX).unwrap()),
        vec![b"durable".to_vec()]
    );
    assert!(spool.append(b"works again").is_ok());
}
