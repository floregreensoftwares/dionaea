//! Moves records from the spool into ClickHouse: one thread reads what is
//! durable, joins the records into one insert, retries until the store takes
//! it, and only then commits the spool, so delivery is at-least-once.
//!
//! **Duplicates.** Every insert carries a deduplication token made from the
//! positions of the first and last record it holds. Retrying the same batch
//! (a lost response, a timeout) reuses the token, so ClickHouse stores it
//! once. After a restart the batch is read again from the last commit; it has
//! the same boundaries unless more records became durable meanwhile, in which
//! case the token differs and the records of the earlier part can be stored
//! twice. That is the bound: at most one batch of duplicates per restart.
//!
//! **Failures.** A transient error (the store is down or slow) is retried
//! with growing waits and never loses anything: the records stay in the
//! spool. A permanent error (the server rejects the rows) is isolated: a
//! batch of several records is retried record by record, and a record that is
//! still rejected is skipped and counted, so one poison request cannot block
//! the queue forever.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dionaea_spool::{Record, Spool};
use dionaea_store::{Error, Store};

use crate::sink;

/// Where batches go. [`Store`] is the implementation; tests use their own to
/// make failures happen on demand.
pub trait Destination: Send + Sync + 'static {
    fn insert_event_lines(&self, lines: &str, token: &str) -> Result<(), Error>;
    fn insert_drop_lines(&self, lines: &str, token: &str) -> Result<(), Error>;
}

impl Destination for Store {
    fn insert_event_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        Store::insert_event_lines(self, lines, token)
    }

    fn insert_drop_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        Store::insert_drop_lines(self, lines, token)
    }
}

#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// The most records joined into one insert.
    pub max_records: usize,
    /// About how many bytes of record payload one insert holds.
    pub max_bytes: usize,
    /// The least time between two inserts, so a busy spool makes a few large
    /// parts in ClickHouse instead of many small ones.
    pub min_interval: Duration,
    /// How long to wait when the spool has nothing to read.
    pub poll: Duration,
    /// The first wait after a transient failure; it doubles up to `backoff_max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            max_records: 1000,
            max_bytes: 8 * 1024 * 1024,
            min_interval: Duration::from_millis(200),
            poll: Duration::from_millis(50),
            backoff_min: Duration::from_millis(200),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Counters for metrics and for tests.
#[derive(Debug, Default)]
pub struct BatcherStats {
    batches: AtomicU64,
    event_rows: AtomicU64,
    drop_rows: AtomicU64,
    retries: AtomicU64,
    skipped_records: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl BatcherStats {
    /// Inserts that the store confirmed.
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::Relaxed)
    }

    /// Event rows the store confirmed.
    pub fn event_rows(&self) -> u64 {
        self.event_rows.load(Ordering::Relaxed)
    }

    pub fn drop_rows(&self) -> u64 {
        self.drop_rows.load(Ordering::Relaxed)
    }

    /// Attempts repeated after a transient failure.
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }

    /// Records given up on: unreadable, or rejected by the store for good.
    pub fn skipped_records(&self) -> u64 {
        self.skipped_records.load(Ordering::Relaxed)
    }

    /// The last failure, until the next success clears it.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_error(&self, error: Option<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }
}

/// A running batcher thread.
#[derive(Debug)]
pub struct Batcher {
    stop: Arc<AtomicBool>,
    stats: Arc<BatcherStats>,
    thread: Option<JoinHandle<()>>,
}

impl Batcher {
    /// Starts the thread. It runs until [`stop`](Self::stop) or until dropped.
    pub fn start(spool: Spool, destination: Arc<dyn Destination>, config: BatcherConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(BatcherStats::default());
        let thread = {
            let (stop, stats) = (Arc::clone(&stop), Arc::clone(&stats));
            thread::Builder::new()
                .name("dionaea-batcher".to_owned())
                .spawn(move || {
                    run(&spool, destination.as_ref(), &config, &stop, &stats);
                    // Records read but not confirmed are read again by the next
                    // batcher on this spool, not skipped.
                    spool.rewind();
                })
                .ok()
        };
        Self {
            stop,
            stats,
            thread,
        }
    }

    pub fn stats(&self) -> &BatcherStats {
        &self.stats
    }

    /// Stops after the insert in progress (or the wait after a failure) and
    /// waits for the thread. Records not yet confirmed stay in the spool.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Batcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire)
}

/// Sleeps `total`, waking early when asked to stop.
fn pause(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(10);
    let end = Instant::now() + total;
    while !stopped(stop) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(step));
    }
}

fn run(
    spool: &Spool,
    destination: &dyn Destination,
    config: &BatcherConfig,
    stop: &AtomicBool,
    stats: &BatcherStats,
) {
    let mut backoff = config.backoff_min;
    let mut last_insert = None::<Instant>;
    while !stopped(stop) {
        let records = match spool.read(config.max_records, config.max_bytes) {
            Ok(records) => records,
            Err(error) => {
                stats.set_error(Some(error.to_string()));
                pause(backoff, stop);
                backoff = (backoff * 2).min(config.backoff_max);
                continue;
            }
        };
        let Some(last) = records.last() else {
            pause(config.poll, stop);
            continue;
        };
        if let Some(at) = last_insert {
            pause(config.min_interval.saturating_sub(at.elapsed()), stop);
        }
        let through = last.next;
        match deliver(&records, destination, config, stop, stats, &mut backoff) {
            Delivered::Done => {
                if let Err(error) = spool.commit(through) {
                    stats.set_error(Some(error.to_string()));
                    pause(backoff, stop);
                    // The records were stored; reading again would repeat them
                    // under the same token, which the store ignores.
                    spool.rewind();
                    continue;
                }
                last_insert = Some(Instant::now());
            }
            Delivered::Stopped => return,
        }
    }
}

enum Delivered {
    Done,
    Stopped,
}

fn token(records: &[Record]) -> String {
    let (first, last) = (&records[0].id, &records[records.len() - 1].next);
    format!(
        "{}.{}-{}.{}",
        first.segment, first.offset, last.segment, last.offset
    )
}

/// Joins the lines of `records`, skipping those that cannot be read.
fn join(records: &[Record], stats: &BatcherStats) -> (String, String) {
    let (mut events, mut drops) = (String::new(), String::new());
    for record in records {
        match sink::decode(&record.payload) {
            Some(lines) => {
                events.push_str(lines.events);
                drops.push_str(lines.drops);
            }
            None => {
                stats.skipped_records.fetch_add(1, Ordering::Relaxed);
                stats.set_error(Some("a spool record could not be read".to_owned()));
            }
        }
    }
    (events, drops)
}

fn count_lines(text: &str) -> u64 {
    text.bytes().filter(|&b| b == b'\n').count() as u64
}

/// Inserts the records, retrying transient failures until they succeed or the
/// batcher is stopped, and isolating a permanent failure.
fn deliver(
    records: &[Record],
    destination: &dyn Destination,
    config: &BatcherConfig,
    stop: &AtomicBool,
    stats: &BatcherStats,
    backoff: &mut Duration,
) -> Delivered {
    let (events, drops) = join(records, stats);
    let token = token(records);
    loop {
        let result = destination
            .insert_event_lines(&events, &format!("{token}-e"))
            .and_then(|()| destination.insert_drop_lines(&drops, &format!("{token}-d")));
        match result {
            Ok(()) => {
                stats.batches.fetch_add(1, Ordering::Relaxed);
                stats
                    .event_rows
                    .fetch_add(count_lines(&events), Ordering::Relaxed);
                stats
                    .drop_rows
                    .fetch_add(count_lines(&drops), Ordering::Relaxed);
                stats.set_error(None);
                *backoff = config.backoff_min;
                return Delivered::Done;
            }
            Err(error) if error.is_permanent() => {
                stats.set_error(Some(error.to_string()));
                if records.len() > 1 {
                    for record in records {
                        if let Delivered::Stopped = deliver(
                            std::slice::from_ref(record),
                            destination,
                            config,
                            stop,
                            stats,
                            backoff,
                        ) {
                            return Delivered::Stopped;
                        }
                    }
                } else {
                    stats.skipped_records.fetch_add(1, Ordering::Relaxed);
                }
                return Delivered::Done;
            }
            Err(error) => {
                stats.retries.fetch_add(1, Ordering::Relaxed);
                stats.set_error(Some(error.to_string()));
                pause(*backoff, stop);
                *backoff = (*backoff * 2).min(config.backoff_max);
                if stopped(stop) {
                    return Delivered::Stopped;
                }
            }
        }
    }
}
