//! A durable on-disk queue between the event guard and the store.
//!
//! An event is acknowledged to its sender only after it is in the spool, so
//! the spool must survive a crash and a store outage. The design is small on
//! purpose:
//!
//! - **Append-only segments** (`NNNNNNNNNNNNNNNN.seg`): a header, then
//!   checksummed records ([`format`] has the bytes). A record never spans two
//!   segments.
//! - **Group commit.** [`Spool::append`] hands the record to one writer thread
//!   and waits. The writer takes everything queued while it was busy, writes it
//!   and calls `fsync` once, then wakes every waiter. Under load one `fsync`
//!   serves many appends; with one caller it is one `fsync` per append.
//! - **One consumer** reads records in order with [`Spool::read`] and confirms
//!   them with [`Spool::commit`]. The committed position is stored in a small
//!   file, written atomically; segments entirely behind it are deleted. A crash
//!   between reading and committing replays those records, so delivery is
//!   at-least-once.
//! - **Bounds.** Disk use is capped by a quota ([`SpoolError::Full`]) and the
//!   memory held by queued appends by `max_pending_bytes` ([`SpoolError::Busy`]),
//!   so a slow store cannot grow memory without limit; callers answer `503`.
//! - **Recovery.** Opening scans every segment, cuts off a torn or damaged
//!   tail at the first record that does not check out, and counts the bytes
//!   cut. An advisory lock keeps two processes from using one directory.
//!
//! What a test can show, and what it cannot: killing the process proves that an
//! acknowledged record is found again. Surviving a power cut depends on the
//! disk and filesystem honouring `fsync`, which no test here can show.

mod format;

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use format::{HEADER_LEN, RECORD_OVERHEAD, Read1};

const MARKER: &str = "consumed";
const LOCK: &str = "LOCK";

/// How a spool is laid out and bounded.
#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    /// A new segment starts when the next record would take the active one
    /// past this size.
    pub segment_bytes: u64,
    /// The most disk the spool uses, headers and records together. Past it
    /// appends fail with [`SpoolError::Full`].
    pub quota_bytes: u64,
    /// The largest payload of one record.
    pub max_record_bytes: u32,
    /// The most bytes queued for the writer at once. Past it appends fail
    /// with [`SpoolError::Busy`].
    pub max_pending_bytes: u64,
    /// Whether the writer calls `fsync` before acknowledging. Only tests and
    /// benchmarks of the other costs turn this off.
    pub sync: bool,
}

impl Config {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            segment_bytes: 16 * 1024 * 1024,
            quota_bytes: 1024 * 1024 * 1024,
            max_record_bytes: 1024 * 1024,
            max_pending_bytes: 64 * 1024 * 1024,
            sync: true,
        }
    }
}

/// Where a record starts, or where the next one would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub segment: u64,
    pub offset: u64,
}

/// A record read from the spool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: Position,
    /// Where the record after this one starts; pass it to [`Spool::commit`] to
    /// confirm everything up to and including this record.
    pub next: Position,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpoolError {
    /// The disk quota is used up.
    Full,
    /// Too many bytes are waiting for the writer.
    Busy,
    /// The payload is larger than `max_record_bytes`.
    TooLarge,
    /// The payload is empty.
    Empty,
    /// The spool is shutting down.
    Closed,
    /// Another process holds the directory.
    Locked,
    /// A commit named a position the consumer has not read.
    BadCommit,
    /// The disk failed; the spool refuses further appends.
    Io(String),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("the spool quota is used up"),
            Self::Busy => f.write_str("too many appends are waiting for the writer"),
            Self::TooLarge => f.write_str("the record is larger than the maximum"),
            Self::Empty => f.write_str("the record is empty"),
            Self::Closed => f.write_str("the spool is closed"),
            Self::Locked => f.write_str("another process is using this spool directory"),
            Self::BadCommit => f.write_str("the commit is outside what was read"),
            Self::Io(message) => write!(f, "spool I/O failed: {message}"),
        }
    }
}

impl std::error::Error for SpoolError {}

impl From<io::Error> for SpoolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Counters for metrics and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Disk used by segments, plus records queued for the writer.
    pub bytes_used: u64,
    /// Bytes queued for the writer.
    pub pending_bytes: u64,
    pub segments: u64,
    /// Records acknowledged since the spool was opened.
    pub appended_records: u64,
    pub appended_bytes: u64,
    /// `fsync` calls made for appends since the spool was opened.
    pub syncs: u64,
    /// Bytes cut off damaged or torn segment tails when the spool was opened.
    pub truncated_on_open: u64,
    /// Why the spool refuses appends, once a write has failed; the spool is
    /// then not ready and only reopening it recovers.
    pub failed: Option<String>,
    /// The age of the oldest segment still on disk.
    pub oldest_segment_age: Option<Duration>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn segment_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{sequence:016}.seg"))
}

/// An append waiting to be acknowledged.
#[derive(Default)]
struct Slot {
    result: Mutex<Option<Result<Position, SpoolError>>>,
    done: Condvar,
}

impl Slot {
    fn finish(&self, result: Result<Position, SpoolError>) {
        *lock(&self.result) = Some(result);
        self.done.notify_one();
    }

    fn wait(&self) -> Result<Position, SpoolError> {
        let mut result = lock(&self.result);
        loop {
            if let Some(result) = result.take() {
                return result;
            }
            result = self
                .done
                .wait(result)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct Pending {
    payload: Vec<u8>,
    slot: Arc<Slot>,
}

struct Shared {
    queue: Vec<Pending>,
    pending_bytes: u64,
    used_bytes: u64,
    segments: u64,
    /// The end of the durable data in the active segment.
    durable: Position,
    closing: bool,
    failed: Option<String>,
}

struct Consumer {
    committed: Position,
    read: Position,
}

struct Core {
    config: Config,
    shared: Mutex<Shared>,
    work: Condvar,
    consumer: Mutex<Consumer>,
    appended_records: AtomicU64,
    appended_bytes: AtomicU64,
    syncs: AtomicU64,
    truncated_on_open: u64,
    /// Test hook: 1 makes the next write fail, 2 makes it panic.
    #[cfg(test)]
    fault: std::sync::atomic::AtomicU8,
    _lock: File,
}

/// Stops the writer when the last handle goes away, after it has written and
/// acknowledged what is already queued.
struct WriterHandle {
    core: Arc<Core>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        lock(&self.core.shared).closing = true;
        self.core.work.notify_all();
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// The spool. Cheap to clone; every clone is the same spool.
#[derive(Clone)]
pub struct Spool {
    core: Arc<Core>,
    _writer: Arc<WriterHandle>,
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spool")
            .field("dir", &self.core.config.dir)
            .finish()
    }
}

/// What the writer thread owns.
struct Active {
    file: File,
    sequence: u64,
    len: u64,
}

struct Recovered {
    /// Valid length of each segment, by sequence, oldest first.
    segments: Vec<(u64, u64)>,
    truncated: u64,
}

impl Spool {
    /// Opens the spool in `config.dir`, creating it if needed, recovering what a
    /// crash left and starting the writer.
    pub fn open(config: Config) -> Result<Self, SpoolError> {
        fs::create_dir_all(&config.dir)?;
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(config.dir.join(LOCK))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(SpoolError::Locked),
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }

        let recovered = recover(&config)?;
        let marker = read_marker(&config.dir);
        let committed = initial_committed(marker, &recovered);
        // Segments behind the committed position were consumed; a crash may
        // have left them.
        let mut segments = recovered.segments.clone();
        while let Some(&(sequence, _)) = segments.first() {
            if sequence >= committed.segment {
                break;
            }
            let _ = fs::remove_file(segment_path(&config.dir, sequence));
            segments.remove(0);
        }

        let active = open_active(&config, &segments, committed)?;
        let is_new = !segments.iter().any(|s| s.0 == active.sequence);
        let used: u64 =
            segments.iter().map(|s| s.1).sum::<u64>() + if is_new { active.len } else { 0 };
        let core = Arc::new(Core {
            shared: Mutex::new(Shared {
                queue: Vec::new(),
                pending_bytes: 0,
                used_bytes: used,
                segments: (segments.len() + usize::from(is_new)) as u64,
                durable: Position {
                    segment: active.sequence,
                    offset: active.len,
                },
                closing: false,
                failed: None,
            }),
            work: Condvar::new(),
            consumer: Mutex::new(Consumer {
                committed,
                read: committed,
            }),
            appended_records: AtomicU64::new(0),
            appended_bytes: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            truncated_on_open: recovered.truncated,
            #[cfg(test)]
            fault: std::sync::atomic::AtomicU8::new(0),
            config,
            _lock: lock_file,
        });
        let writer_core = Arc::clone(&core);
        let thread = std::thread::Builder::new()
            .name("spool-writer".to_owned())
            .spawn(move || {
                // A panic must not leave appenders waiting forever.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_writer(&writer_core, active);
                }));
                if outcome.is_err() {
                    fail_all(&writer_core, "the spool writer panicked", &[]);
                }
            })?;
        let writer = Arc::new(WriterHandle {
            core: Arc::clone(&core),
            thread: Mutex::new(Some(thread)),
        });
        Ok(Self {
            core,
            _writer: writer,
        })
    }

    /// Writes one record and returns once it is durable. Blocks the caller for
    /// the duration of a group commit; async callers run it on a blocking pool.
    pub fn append(&self, payload: impl Into<Vec<u8>>) -> Result<Position, SpoolError> {
        let payload: Vec<u8> = payload.into();
        let config = &self.core.config;
        if payload.is_empty() {
            return Err(SpoolError::Empty);
        }
        if payload.len() > config.max_record_bytes as usize {
            return Err(SpoolError::TooLarge);
        }
        let size = RECORD_OVERHEAD + payload.len() as u64;
        let slot = Arc::new(Slot::default());
        {
            let mut shared = lock(&self.core.shared);
            if let Some(message) = &shared.failed {
                return Err(SpoolError::Io(message.clone()));
            }
            if shared.closing {
                return Err(SpoolError::Closed);
            }
            if shared.pending_bytes + size > config.max_pending_bytes {
                return Err(SpoolError::Busy);
            }
            if shared.used_bytes + size > config.quota_bytes {
                return Err(SpoolError::Full);
            }
            shared.pending_bytes += size;
            shared.used_bytes += size;
            shared.queue.push(Pending {
                payload,
                slot: Arc::clone(&slot),
            });
        }
        self.core.work.notify_one();
        slot.wait()
    }

    /// Reads up to `max_records` records (and about `max_bytes` of payload, but
    /// always at least one record when one is available) from where the last
    /// read stopped. Only durable records are returned.
    pub fn read(&self, max_records: usize, max_bytes: usize) -> Result<Vec<Record>, SpoolError> {
        let durable = lock(&self.core.shared).durable;
        let max_record = self.core.config.max_record_bytes;
        let mut consumer = lock(&self.core.consumer);
        let mut position = consumer.read;
        let mut records = Vec::new();
        let mut bytes = 0usize;

        'segments: while position.segment <= durable.segment {
            if records.len() >= max_records || (bytes >= max_bytes && !records.is_empty()) {
                break;
            }
            let path = segment_path(&self.core.config.dir, position.segment);
            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && position.segment < durable.segment =>
                {
                    position = Position {
                        segment: position.segment + 1,
                        offset: HEADER_LEN,
                    };
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let sealed = position.segment < durable.segment;
            let bound = if sealed {
                file.metadata()?.len()
            } else {
                durable.offset
            };
            file.seek(SeekFrom::Start(position.offset))?;
            let mut reader = BufReader::new(file);
            while position.offset < bound {
                if records.len() >= max_records || (bytes >= max_bytes && !records.is_empty()) {
                    break 'segments;
                }
                match format::read_record(&mut reader, max_record)? {
                    Read1::Record(payload, taken) => {
                        let id = position;
                        position.offset += taken;
                        bytes += payload.len();
                        records.push(Record {
                            id,
                            next: position,
                            payload,
                        });
                    }
                    // Recovery cut every damaged tail, so a record that does
                    // not check out here means the file changed under us.
                    Read1::End => {
                        return Err(SpoolError::Io(format!(
                            "damaged record in segment {} at offset {}",
                            position.segment, position.offset
                        )));
                    }
                }
            }
            if sealed {
                position = Position {
                    segment: position.segment + 1,
                    offset: HEADER_LEN,
                };
            } else {
                break;
            }
        }
        consumer.read = position;
        Ok(records)
    }

    /// Confirms every record before `through` (a [`Record::next`]). The
    /// position is stored before any segment is deleted, so a crash in between
    /// leaves only extra files, never a gap.
    pub fn commit(&self, through: Position) -> Result<(), SpoolError> {
        let durable = lock(&self.core.shared).durable;
        let mut consumer = lock(&self.core.consumer);
        if through < consumer.committed || through > consumer.read {
            return Err(SpoolError::BadCommit);
        }
        let mut committed = through;
        if committed.segment < durable.segment {
            let len = fs::metadata(segment_path(&self.core.config.dir, committed.segment))
                .map(|m| m.len())
                .unwrap_or(committed.offset);
            if committed.offset >= len {
                committed = Position {
                    segment: committed.segment + 1,
                    offset: HEADER_LEN,
                };
            }
        }
        write_marker(&self.core.config.dir, committed)?;
        consumer.committed = committed;
        if consumer.read < committed {
            consumer.read = committed;
        }

        let mut freed = 0;
        let mut removed = 0;
        for sequence in segment_sequences(&self.core.config.dir)? {
            if sequence >= committed.segment {
                break;
            }
            let path = segment_path(&self.core.config.dir, sequence);
            if let Ok(meta) = fs::metadata(&path) {
                freed += meta.len();
            }
            fs::remove_file(&path)?;
            removed += 1;
        }
        if removed > 0 {
            let mut shared = lock(&self.core.shared);
            shared.used_bytes = shared.used_bytes.saturating_sub(freed);
            shared.segments = shared.segments.saturating_sub(removed);
        }
        Ok(())
    }

    /// Starts the next [`read`](Self::read) again from the committed position,
    /// for when delivery of what was read failed.
    pub fn rewind(&self) {
        let mut consumer = lock(&self.core.consumer);
        consumer.read = consumer.committed;
    }

    pub fn stats(&self) -> Stats {
        let (bytes_used, pending_bytes, segments, failed) = {
            let shared = lock(&self.core.shared);
            (
                shared.used_bytes,
                shared.pending_bytes,
                shared.segments,
                shared.failed.clone(),
            )
        };
        let oldest_segment_age = first_sequence(&self.core.config.dir)
            .ok()
            .flatten()
            .and_then(|sequence| fs::metadata(segment_path(&self.core.config.dir, sequence)).ok())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok());
        Stats {
            bytes_used,
            pending_bytes,
            segments,
            appended_records: self.core.appended_records.load(Ordering::Relaxed),
            appended_bytes: self.core.appended_bytes.load(Ordering::Relaxed),
            syncs: self.core.syncs.load(Ordering::Relaxed),
            truncated_on_open: self.core.truncated_on_open,
            failed,
            oldest_segment_age,
        }
    }
}

// The writer

fn run_writer(core: &Arc<Core>, mut active: Active) {
    loop {
        let batch = {
            let mut shared = lock(&core.shared);
            while shared.queue.is_empty() && !shared.closing {
                shared = core
                    .work
                    .wait(shared)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if shared.queue.is_empty() {
                return;
            }
            std::mem::take(&mut shared.queue)
        };
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_batch(core, &mut active, &batch)
        }));
        match written {
            Err(_) => {
                fail_all(core, "the spool writer panicked", &batch);
                return;
            }
            Ok(result) => match result {
                Ok(positions) => {
                    let bytes: u64 = batch
                        .iter()
                        .map(|p| RECORD_OVERHEAD + p.payload.len() as u64)
                        .sum();
                    {
                        let mut shared = lock(&core.shared);
                        shared.pending_bytes = shared.pending_bytes.saturating_sub(bytes);
                        shared.durable = Position {
                            segment: active.sequence,
                            offset: active.len,
                        };
                    }
                    core.appended_records
                        .fetch_add(batch.len() as u64, Ordering::Relaxed);
                    core.appended_bytes.fetch_add(bytes, Ordering::Relaxed);
                    for (pending, position) in batch.iter().zip(positions) {
                        pending.slot.finish(Ok(position));
                    }
                }
                Err(error) => {
                    fail_all(core, &error.to_string(), &batch);
                    return;
                }
            },
        }
    }
}

/// Puts the spool in the failed state and answers every append that is
/// waiting, `batch` (the one being written) and whatever is queued. The only
/// way out is to open the spool again, which cuts a torn tail.
fn fail_all(core: &Core, message: &str, batch: &[Pending]) {
    let queued = {
        let mut shared = lock(&core.shared);
        shared.failed = Some(message.to_owned());
        std::mem::take(&mut shared.queue)
    };
    for pending in batch.iter().chain(queued.iter()) {
        pending.slot.finish(Err(SpoolError::Io(message.to_owned())));
    }
}

/// Writes a batch, rolling to a new segment when one fills, and makes it
/// durable with one `fsync` of the last file (earlier ones are synced when they
/// are sealed). Returns each record's position.
fn write_batch(core: &Core, active: &mut Active, batch: &[Pending]) -> io::Result<Vec<Position>> {
    #[cfg(test)]
    match core.fault.load(Ordering::Relaxed) {
        1 => return Err(io::Error::other("injected failure")),
        2 => panic!("injected panic"),
        _ => {}
    }
    let config = &core.config;
    let mut positions = Vec::with_capacity(batch.len());
    let mut buffer = Vec::new();
    for pending in batch {
        let size = RECORD_OVERHEAD + pending.payload.len() as u64;
        if active.len + buffer.len() as u64 + size > config.segment_bytes
            && active.len + buffer.len() as u64 > HEADER_LEN
        {
            active.file.write_all(&buffer)?;
            active.len += buffer.len() as u64;
            buffer.clear();
            seal_and_roll(core, active)?;
        }
        positions.push(Position {
            segment: active.sequence,
            offset: active.len + buffer.len() as u64,
        });
        format::encode_record(&mut buffer, &pending.payload);
    }
    active.file.write_all(&buffer)?;
    active.len += buffer.len() as u64;
    if config.sync {
        active.file.sync_data()?;
        core.syncs.fetch_add(1, Ordering::Relaxed);
    }
    Ok(positions)
}

fn seal_and_roll(core: &Core, active: &mut Active) -> io::Result<()> {
    let config = &core.config;
    if config.sync {
        active.file.sync_data()?;
    }
    let sequence = active.sequence + 1;
    let mut file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(segment_path(&config.dir, sequence))?;
    file.write_all(&format::encode_header(sequence))?;
    if config.sync {
        file.sync_data()?;
        sync_dir(&config.dir)?;
    }
    *active = Active {
        file,
        sequence,
        len: HEADER_LEN,
    };
    let mut shared = lock(&core.shared);
    shared.used_bytes += HEADER_LEN;
    shared.segments += 1;
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows has no way to flush a directory through the standard library; new
/// files there are made durable by flushing the file itself.
#[cfg(not(unix))]
fn sync_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

// Opening

/// The sequences of the segment files in `dir`, oldest first.
fn segment_sequences(dir: &Path) -> io::Result<Vec<u64>> {
    let mut sequences = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(stem) = name.strip_suffix(".seg")
            && let Ok(sequence) = stem.parse::<u64>()
        {
            sequences.push(sequence);
        }
    }
    sequences.sort_unstable();
    Ok(sequences)
}

fn first_sequence(dir: &Path) -> io::Result<Option<u64>> {
    Ok(segment_sequences(dir)?.first().copied())
}

/// Validates every segment, cutting off whatever follows the last valid
/// record.
fn recover(config: &Config) -> io::Result<Recovered> {
    let mut segments = Vec::new();
    let mut truncated = 0;
    for sequence in segment_sequences(&config.dir)? {
        let path = segment_path(&config.dir, sequence);
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let file_len = file.metadata()?.len();
        let mut header = [0u8; HEADER_LEN as usize];
        let header_ok = io::Read::read_exact(&mut file, &mut header).is_ok()
            && format::decode_header(&header) == Some(sequence);
        if !header_ok {
            // Not even a header: a file created at the moment of a crash, or
            // damage. Nothing in it can be trusted.
            drop(file);
            fs::remove_file(&path)?;
            truncated += file_len;
            continue;
        }
        let mut reader = BufReader::new(file);
        let mut valid = HEADER_LEN;
        while let Read1::Record(_, taken) =
            format::read_record(&mut reader, config.max_record_bytes)?
        {
            valid += taken;
        }
        let file = reader.into_inner();
        if valid < file_len {
            file.set_len(valid)?;
            file.sync_all()?;
            truncated += file_len - valid;
        }
        segments.push((sequence, valid));
    }
    Ok(Recovered {
        segments,
        truncated,
    })
}

/// The segment the writer appends to: the newest if it has room, else a new
/// one after it.
fn open_active(
    config: &Config,
    segments: &[(u64, u64)],
    committed: Position,
) -> io::Result<Active> {
    if let Some(&(sequence, len)) = segments.last()
        && len < config.segment_bytes
    {
        let file = OpenOptions::new()
            .append(true)
            .open(segment_path(&config.dir, sequence))?;
        return Ok(Active {
            file,
            sequence,
            len,
        });
    }
    // With no segment left, the first new one is where the consumer expects it.
    let sequence = segments.last().map_or(committed.segment, |s| s.0 + 1);
    let mut file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(segment_path(&config.dir, sequence))?;
    file.write_all(&format::encode_header(sequence))?;
    if config.sync {
        file.sync_data()?;
        sync_dir(&config.dir)?;
    }
    Ok(Active {
        file,
        sequence,
        len: HEADER_LEN,
    })
}

/// Where the consumer resumes: the stored position, clamped to what exists.
fn initial_committed(marker: Option<Position>, recovered: &Recovered) -> Position {
    let Some(&(first, _)) = recovered.segments.first() else {
        return marker.map_or(
            Position {
                segment: 1,
                offset: HEADER_LEN,
            },
            |m| Position {
                segment: m.segment,
                offset: HEADER_LEN,
            },
        );
    };
    let Some(marker) = marker else {
        return Position {
            segment: first,
            offset: HEADER_LEN,
        };
    };
    if marker.segment < first {
        return Position {
            segment: first,
            offset: HEADER_LEN,
        };
    }
    match recovered.segments.iter().find(|s| s.0 == marker.segment) {
        Some(&(_, len)) if marker.offset <= len && marker.offset >= HEADER_LEN => marker,
        Some(&(_, len)) => Position {
            segment: marker.segment,
            offset: len.max(HEADER_LEN),
        },
        // A marker past the newest segment: the segments it pointed into are
        // gone, so start from the oldest that remains (at-least-once).
        None => Position {
            segment: first,
            offset: HEADER_LEN,
        },
    }
}

// The consumer marker

fn encode_marker(position: Position) -> [u8; 28] {
    let mut bytes = [0u8; 28];
    bytes[..4].copy_from_slice(b"DNSC");
    bytes[4] = 1;
    bytes[8..16].copy_from_slice(&position.segment.to_le_bytes());
    bytes[16..24].copy_from_slice(&position.offset.to_le_bytes());
    let crc = format::crc32(&bytes[..24]);
    bytes[24..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode_marker(bytes: &[u8]) -> Option<Position> {
    if bytes.len() != 28 || bytes[..4] != *b"DNSC" || bytes[4] != 1 {
        return None;
    }
    let crc = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
    if format::crc32(&bytes[..24]) != crc {
        return None;
    }
    Some(Position {
        segment: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        offset: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
    })
}

fn read_marker(dir: &Path) -> Option<Position> {
    decode_marker(&fs::read(dir.join(MARKER)).ok()?)
}

/// Writes the marker atomically: a temporary file, flushed, then renamed over
/// the old one.
fn write_marker(dir: &Path, position: Position) -> io::Result<()> {
    let temporary = dir.join("consumed.tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(&encode_marker(position))?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join(MARKER))?;
    sync_dir(dir)
}

#[cfg(test)]
mod tests;
