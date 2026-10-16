//! A bounded memory of recent event ids, so an event an SDK sends again after
//! a lost response is dropped instead of stored twice.
//!
//! An id is remembered only after its event is durable ([`Deduper::remember`]),
//! never before: remembering an id whose request then failed with `503` would
//! make the retry look like a duplicate and lose the event.
//!
//! The memory lives in the process and, with a journal ([`Deduper::open`]), in
//! a file next to the spool, so a restart does not forget it: ids are appended
//! as they are remembered, made durable by [`Deduper::sync`] (the caller runs it
//! about once a second, so a crash loses at most that long), and the file is
//! rewritten from memory when it has grown past twice the capacity.
//!
//! It is still per node: a retry that reaches another replica is stored again.
//! Routing an SDK to the same replica (for example by hashing the write key)
//! closes that; the insert's own deduplication only covers a batch read again
//! from the spool.

use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// One journal record: the id (big endian) and when it was remembered.
const RECORD: usize = 16 + 8;

#[derive(Debug, Default)]
struct Window {
    ids: HashSet<u128>,
    order: VecDeque<(u128, u64)>,
}

#[derive(Debug)]
struct Journal {
    path: PathBuf,
    writer: BufWriter<File>,
    /// Records in the file, including ones the memory has forgotten.
    records: usize,
}

/// Remembers up to `capacity` ids for `window_ms` milliseconds each.
#[derive(Debug)]
pub struct Deduper {
    capacity: usize,
    window_ms: u64,
    state: Mutex<Window>,
    journal: Mutex<Option<Journal>>,
}

impl Deduper {
    /// A memory kept only in the process. `capacity` 0 remembers nothing,
    /// which turns the check off.
    pub fn new(capacity: usize, window_ms: u64) -> Self {
        Self {
            capacity,
            window_ms,
            state: Mutex::new(Window::default()),
            journal: Mutex::new(None),
        }
    }

    /// A memory that also lives in the file at `path`: what is there and not
    /// yet expired at `now_ms` is loaded (the newest `capacity` ids), and the
    /// file is rewritten without the rest.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or written.
    pub fn open(capacity: usize, window_ms: u64, path: &Path, now_ms: u64) -> io::Result<Self> {
        let deduper = Self::new(capacity, window_ms);
        if capacity == 0 {
            return Ok(deduper);
        }
        {
            let mut state = deduper.state.lock().unwrap_or_else(PoisonError::into_inner);
            match File::open(path) {
                Ok(file) => load(BufReader::new(file), &mut state)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            deduper.expire(&mut state, now_ms);
            deduper.trim(&mut state);
            let journal = rewrite(path, &state)?;
            *deduper
                .journal
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(journal);
        }
        Ok(deduper)
    }

    /// Whether `id` was remembered and has not expired at `now_ms`.
    pub fn contains(&self, id: u128, now_ms: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.expire(&mut state, now_ms);
        state.ids.contains(&id)
    }

    /// Remembers `ids` as seen at `now_ms`. When full, the oldest are forgotten.
    pub fn remember(&self, ids: impl IntoIterator<Item = u128>, now_ms: u64) {
        if self.capacity == 0 {
            return;
        }
        let mut fresh = Vec::new();
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            self.expire(&mut state, now_ms);
            for id in ids {
                if state.ids.insert(id) {
                    state.order.push_back((id, now_ms));
                    fresh.push(id);
                }
            }
            self.trim(&mut state);
        }
        if fresh.is_empty() {
            return;
        }
        let mut journal = self.journal.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(journal) = journal.as_mut() {
            for id in fresh {
                let mut record = [0u8; RECORD];
                record[..16].copy_from_slice(&id.to_be_bytes());
                record[16..].copy_from_slice(&now_ms.to_be_bytes());
                // A failed write is noticed by `sync`; the memory is right.
                let _ = journal.writer.write_all(&record);
                journal.records += 1;
            }
        }
    }

    /// Makes the journal durable, and rewrites it from memory when it has
    /// grown past twice the capacity. Without a journal it does nothing.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn sync(&self) -> io::Result<()> {
        let mut journal = self.journal.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(current) = journal.as_mut() else {
            return Ok(());
        };
        if current.records > self.capacity.saturating_mul(2) {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            *current = rewrite(&current.path.clone(), &state)?;
            return Ok(());
        }
        current.writer.flush()?;
        current.writer.get_ref().sync_data()
    }

    fn expire(&self, state: &mut Window, now_ms: u64) {
        while let Some(&(id, at)) = state.order.front() {
            if now_ms.saturating_sub(at) < self.window_ms {
                break;
            }
            state.order.pop_front();
            state.ids.remove(&id);
        }
    }

    fn trim(&self, state: &mut Window) {
        while state.ids.len() > self.capacity {
            if let Some((oldest, _)) = state.order.pop_front() {
                state.ids.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// How many ids are remembered.
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ids
            .len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for Deduper {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

/// Reads whole records; a torn last record is ignored.
fn load(mut reader: impl Read, state: &mut Window) -> io::Result<()> {
    let mut record = [0u8; RECORD];
    loop {
        match reader.read_exact(&mut record) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let id = u128::from_be_bytes(record[..16].try_into().unwrap_or([0; 16]));
        let at = u64::from_be_bytes(record[16..].try_into().unwrap_or([0; 8]));
        if state.ids.insert(id) {
            state.order.push_back((id, at));
        }
    }
}

/// Writes the memory to a new file, replaces the journal with it and opens it
/// for appending.
fn rewrite(path: &Path, state: &Window) -> io::Result<Journal> {
    let temporary = path.with_extension("tmp");
    {
        let mut file = BufWriter::new(File::create(&temporary)?);
        for (id, at) in &state.order {
            file.write_all(&id.to_be_bytes())?;
            file.write_all(&at.to_be_bytes())?;
        }
        file.flush()?;
        file.get_ref().sync_all()?;
    }
    fs::rename(&temporary, path)?;
    let file = OpenOptions::new().append(true).open(path)?;
    Ok(Journal {
        path: path.to_owned(),
        writer: BufWriter::with_capacity(64 * 1024, file),
        records: state.order.len(),
    })
}
