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

use std::collections::HashSet;
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

    /// Which of these (site, event id) pairs another node already stored.
    /// The default knows none, which turns the cross-node check off.
    fn known_event_ids(&self, _keys: &[(u32, &str)]) -> Result<HashSet<(u32, String)>, Error> {
        Ok(HashSet::new())
    }

    /// Records the ids of events that were stored. The default records none.
    fn insert_event_id_lines(&self, _lines: &str, _token: &str) -> Result<(), Error> {
        Ok(())
    }
}

impl Destination for Store {
    fn insert_event_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        Store::insert_event_lines(self, lines, token)
    }

    fn insert_drop_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        Store::insert_drop_lines(self, lines, token)
    }

    fn known_event_ids(&self, keys: &[(u32, &str)]) -> Result<HashSet<(u32, String)>, Error> {
        Store::known_event_ids(self, keys)
    }

    fn insert_event_id_lines(&self, lines: &str, token: &str) -> Result<(), Error> {
        Store::insert_event_id_lines(self, lines, token)
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
    /// Whether to leave out events another node already stored, and record
    /// the ids of the ones this node stores, through the store.
    pub shared_dedup: bool,
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
            shared_dedup: true,
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
    duplicates: AtomicU64,
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

    /// Events left out because another node, or an earlier batch, already
    /// stored them.
    pub fn duplicates(&self) -> u64 {
        self.duplicates.load(Ordering::Relaxed)
    }

    /// Records, or the event or drop lines of a record, given up on:
    /// unreadable, or rejected by the store for good.
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
    ///
    /// # Errors
    ///
    /// When the operating system cannot start a thread.
    pub fn start(
        spool: Spool,
        destination: Arc<dyn Destination>,
        config: BatcherConfig,
    ) -> std::io::Result<Self> {
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
                })?
        };
        Ok(Self {
            stop,
            stats,
            thread: Some(thread),
        })
    }

    pub fn stats(&self) -> &BatcherStats {
        &self.stats
    }

    /// A handle to the counters that outlives the batcher, for the metrics.
    pub fn stats_handle(&self) -> Arc<BatcherStats> {
        Arc::clone(&self.stats)
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

/// The two kinds of line a record carries. Each is inserted, retried and, if
/// rejected, isolated on its own, so a problem with one never repeats the
/// other.
#[derive(Clone, Copy)]
enum Part {
    Events,
    Drops,
}

fn token(records: &[Record], part: Part) -> String {
    let suffix = match part {
        Part::Events => 'e',
        Part::Drops => 'd',
    };
    positions(records, suffix)
}

/// The token under which the ids of a batch's events are recorded.
fn ids_token(records: &[Record]) -> String {
    positions(records, 'i')
}

fn positions(records: &[Record], suffix: char) -> String {
    let (first, last) = (&records[0].id, &records[records.len() - 1].next);
    format!(
        "{}.{}-{}.{}-{suffix}",
        first.segment, first.offset, last.segment, last.offset
    )
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
    for record in records {
        if sink::decode(&record.payload).is_none() {
            stats.skipped_records.fetch_add(1, Ordering::Relaxed);
            stats.set_error(Some("a spool record could not be read".to_owned()));
        }
    }
    for part in [Part::Events, Part::Drops] {
        if let Delivered::Stopped =
            deliver_part(records, part, destination, config, stop, stats, backoff)
        {
            return Delivered::Stopped;
        }
    }
    Delivered::Done
}

/// How an attempt that may be repeated ended.
enum Attempt<T> {
    Done(T),
    /// The store refused it for good.
    Permanent(Error),
    /// The batcher was asked to stop while waiting to try again.
    Stopped,
}

/// Runs `operation`, repeating transient failures with growing waits.
fn retry<T>(
    config: &BatcherConfig,
    stop: &AtomicBool,
    stats: &BatcherStats,
    backoff: &mut Duration,
    mut operation: impl FnMut() -> Result<T, Error>,
) -> Attempt<T> {
    loop {
        match operation() {
            Ok(value) => return Attempt::Done(value),
            Err(error) if error.is_permanent() => return Attempt::Permanent(error),
            Err(error) => {
                stats.retries.fetch_add(1, Ordering::Relaxed);
                stats.set_error(Some(error.to_string()));
                pause(*backoff, stop);
                *backoff = (*backoff * 2).min(config.backoff_max);
                if stopped(stop) {
                    return Attempt::Stopped;
                }
            }
        }
    }
}

/// Removes from `lines` the events another node already stored (their ids are
/// in the store's table of recent ids) and the ones repeated inside the batch.
/// Returns the lines to insert, the keys of those events (to record once they
/// are stored) and how many were removed.
/// What is left of a batch after the events stored elsewhere are removed.
struct Filtered {
    lines: String,
    keys: Vec<(u32, String)>,
    removed: u64,
}

fn remove_known(destination: &dyn Destination, lines: &str) -> Result<Filtered, Error> {
    let entries: Vec<(&str, Option<(u32, &str)>)> = lines
        .split_inclusive('\n')
        .map(|line| (line, dionaea_store::event_key(line)))
        .collect();

    // The first occurrence of each key in this batch.
    let mut first_seen: HashSet<(u32, String)> = HashSet::with_capacity(entries.len());
    let mut unique: Vec<(u32, String)> = Vec::new();
    for (_, key) in &entries {
        if let Some((site, id)) = key {
            let key = (*site, id.to_ascii_lowercase());
            if first_seen.insert(key.clone()) {
                unique.push(key);
            }
        }
    }
    let mut known: HashSet<(u32, String)> = HashSet::new();
    for chunk in unique.chunks(dionaea_store::MAX_IDS_PER_LOOKUP) {
        let asked: Vec<(u32, &str)> = chunk.iter().map(|(s, i)| (*s, i.as_str())).collect();
        known.extend(destination.known_event_ids(&asked)?);
    }

    let mut kept = String::with_capacity(lines.len());
    let mut kept_keys = Vec::new();
    let mut taken: HashSet<(u32, String)> = HashSet::new();
    let mut removed = 0;
    for (line, key) in entries {
        let Some((site, id)) = key else {
            kept.push_str(line);
            continue;
        };
        let key = (site, id.to_ascii_lowercase());
        if known.contains(&key) || !taken.insert(key.clone()) {
            removed += 1;
            continue;
        }
        kept.push_str(line);
        kept_keys.push(key);
    }
    Ok(Filtered {
        lines: kept,
        keys: kept_keys,
        removed,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn deliver_part(
    records: &[Record],
    part: Part,
    destination: &dyn Destination,
    config: &BatcherConfig,
    stop: &AtomicBool,
    stats: &BatcherStats,
    backoff: &mut Duration,
) -> Delivered {
    let lines: String = records
        .iter()
        .filter_map(|record| sink::decode(&record.payload))
        .map(|lines| match part {
            Part::Events => lines.events,
            Part::Drops => lines.drops,
        })
        .collect();
    if lines.is_empty() {
        return Delivered::Done;
    }
    let token = token(records, part);

    // Events another node already stored are left out; a failure to ask is
    // retried, never answered by inserting without looking. Only a request the
    // store rejects for good lets the batch through unfiltered, since storing
    // an event twice is better than losing it.
    let (lines, keys) = if matches!(part, Part::Events) && config.shared_dedup {
        match retry(config, stop, stats, backoff, || {
            remove_known(destination, &lines)
        }) {
            Attempt::Done(filtered) => {
                stats
                    .duplicates
                    .fetch_add(filtered.removed, Ordering::Relaxed);
                (filtered.lines, filtered.keys)
            }
            Attempt::Permanent(error) => {
                stats.set_error(Some(error.to_string()));
                (lines, Vec::new())
            }
            Attempt::Stopped => return Delivered::Stopped,
        }
    } else {
        (lines, Vec::new())
    };
    if lines.is_empty() {
        // Every event of the batch was already stored elsewhere.
        stats.set_error(None);
        return Delivered::Done;
    }

    let inserted = retry(config, stop, stats, backoff, || match part {
        Part::Events => destination.insert_event_lines(&lines, &token),
        Part::Drops => destination.insert_drop_lines(&lines, &token),
    });
    match inserted {
        Attempt::Done(()) => {
            stats.batches.fetch_add(1, Ordering::Relaxed);
            let rows = match part {
                Part::Events => &stats.event_rows,
                Part::Drops => &stats.drop_rows,
            };
            rows.fetch_add(count_lines(&lines), Ordering::Relaxed);
            stats.set_error(None);
            *backoff = config.backoff_min;
        }
        Attempt::Permanent(error) => {
            stats.set_error(Some(error.to_string()));
            if records.len() > 1 {
                for record in records {
                    let one = std::slice::from_ref(record);
                    if let Delivered::Stopped =
                        deliver_part(one, part, destination, config, stop, stats, backoff)
                    {
                        return Delivered::Stopped;
                    }
                }
            } else {
                stats.skipped_records.fetch_add(1, Ordering::Relaxed);
            }
            return Delivered::Done;
        }
        Attempt::Stopped => return Delivered::Stopped,
    }

    // Record the ids only now that the events are stored: an id recorded
    // earlier would make the retry of an unstored event look like a duplicate.
    if !keys.is_empty() {
        let ids =
            Store::encode_event_ids(keys.iter().map(|(site, id)| (*site, id.as_str())), now_ms());
        let ids_token = ids_token(records);
        // Retried like any insert, but an id that cannot be recorded for good
        // only loses the cross-node check for these events.
        if let Ok(ids) = ids {
            match retry(config, stop, stats, backoff, || {
                destination.insert_event_id_lines(&ids, &ids_token)
            }) {
                Attempt::Stopped => return Delivered::Stopped,
                Attempt::Permanent(error) => stats.set_error(Some(error.to_string())),
                Attempt::Done(()) => {}
            }
        }
    }
    Delivered::Done
}
