//! Turns the stack of an `app_error` event back into the code the developer
//! wrote, and gives the error its fingerprint, between the spool and
//! ClickHouse. It runs on the batcher thread, never on a request: an event is
//! acknowledged once it is durable and symbolicated afterwards, so a flood of
//! errors can slow the move to ClickHouse by at most [`SymbolConfig::deadline`]
//! per batch and never a request.
//!
//! **What is looked up.** For a script frame (a module that is a path, an URL
//! or ends in a script extension) the source map of the event's release named
//! like the module (`static/js/main.js` is `static/js/main.js.map`, or
//! `main.js.map`). For any other frame of an Android event the release's
//! `mapping.txt`. Files come from the index in the metadata store and the
//! bytes from the artifact store.
//!
//! **What is stored.** The event keeps the translated `frames`, and gains the
//! string properties `fingerprint` (`v1`) and `symbols`: `ok` (every file
//! needed was there), `partial` (some were not), `missing` (no file for the
//! release), `failed` (a file was there and could not be used), `none`
//! (nothing needed or mapped) and `skipped` (the batch ran out of time). An
//! event whose symbols are missing is stored with its raw frames and flagged;
//! it is never translated later.
//!
//! **Bounds.** Parsed files live in a cache limited in bytes, and the number
//! of entries follows from it (a missing file also costs bytes), so release
//! names a sender makes up cannot grow memory. A file larger than the cache is
//! `failed`. At most [`MAX_FILES_PER_EVENT`] files are consulted for one
//! event. The workers are threads of the batch, started and ended with it.

use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use dionaea_artifacts::{ArtifactId, ArtifactStore};
use dionaea_core::MAX_FRAMES_CHARS;
use dionaea_health::fingerprint;
use dionaea_health::frames::{self, Frame};
use dionaea_health::proguard::{self, ProguardMap};
use dionaea_health::sourcemap::{self, SourceMap};
use dionaea_meta::{SymbolIndex, SymbolKind, is_release, is_symbol_name};
use serde_json::Value;

/// Most files one event may make the cache open.
pub const MAX_FILES_PER_EVENT: usize = 8;
/// The largest file read, the same as the upload limit.
const MAX_FILE_BYTES: usize = 64 << 20;
/// The name of an Android release's mapping file.
pub const MAPPING_NAME: &str = "mapping.txt";
/// What an entry costs besides its file, so entries for missing files count.
const ENTRY_OVERHEAD: usize = 256;

#[derive(Debug, Clone)]
pub struct SymbolConfig {
    /// Threads that work on a batch, the batcher's own included.
    pub workers: usize,
    /// How long a batch may spend on symbolication; events not reached are
    /// stored with their raw frames and `symbols = skipped`.
    pub deadline: Duration,
    /// Memory for parsed files and the entries standing for missing ones.
    pub cache_bytes: usize,
    /// How long an entry is trusted before the index is asked again, which
    /// also bounds how soon an uploaded or replaced file is noticed and how
    /// long a missing or unusable file is remembered.
    pub recheck: Duration,
}

impl Default for SymbolConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            deadline: Duration::from_secs(2),
            cache_bytes: 128 << 20,
            recheck: Duration::from_secs(30),
        }
    }
}

/// Counters for the metrics and for tests.
#[derive(Debug, Default)]
pub struct SymbolStats {
    ok: AtomicU64,
    partial: AtomicU64,
    missing: AtomicU64,
    failed: AtomicU64,
    none: AtomicU64,
    skipped: AtomicU64,
    loads: AtomicU64,
    hits: AtomicU64,
    index_errors: AtomicU64,
    panics: AtomicU64,
    micros: AtomicU64,
    cache_bytes: AtomicUsize,
    cache_entries: AtomicUsize,
}

impl SymbolStats {
    /// Events by the `symbols` value they were stored with.
    pub fn outcomes(&self) -> [(&'static str, u64); 6] {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        [
            ("ok", get(&self.ok)),
            ("partial", get(&self.partial)),
            ("missing", get(&self.missing)),
            ("failed", get(&self.failed)),
            ("none", get(&self.none)),
            ("skipped", get(&self.skipped)),
        ]
    }

    /// Files read from the artifact store and parsed.
    pub fn loads(&self) -> u64 {
        self.loads.load(Ordering::Relaxed)
    }

    /// Lookups answered by the cache.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Times the index of symbol files could not be read.
    pub fn index_errors(&self) -> u64 {
        self.index_errors.load(Ordering::Relaxed)
    }

    /// Events whose symbolication panicked and was contained.
    pub fn panics(&self) -> u64 {
        self.panics.load(Ordering::Relaxed)
    }

    /// Time spent on events, in microseconds, summed over the workers.
    pub fn micros(&self) -> u64 {
        self.micros.load(Ordering::Relaxed)
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes.load(Ordering::Relaxed)
    }

    pub fn cache_entries(&self) -> usize {
        self.cache_entries.load(Ordering::Relaxed)
    }

    fn count(&self, state: State) {
        match state {
            State::Ok => &self.ok,
            State::Partial => &self.partial,
            State::Missing => &self.missing,
            State::Failed => &self.failed,
            State::None => &self.none,
            State::Skipped => &self.skipped,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ok,
    Partial,
    Missing,
    Failed,
    None,
    Skipped,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Missing => "missing",
            Self::Failed => "failed",
            Self::None => "none",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone)]
enum Loaded {
    Source(Arc<SourceMap>),
    Mapping(Arc<ProguardMap>),
    Missing,
    Failed,
}

impl Loaded {
    fn bytes(&self) -> usize {
        match self {
            Self::Source(map) => map.approx_bytes(),
            Self::Mapping(map) => map.approx_bytes(),
            Self::Missing | Self::Failed => 0,
        }
    }

    fn is_usable(&self) -> bool {
        matches!(self, Self::Source(_) | Self::Mapping(_))
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    site: u32,
    release: String,
    kind: SymbolKind,
    name: String,
}

impl Key {
    fn cost(&self) -> usize {
        ENTRY_OVERHEAD + self.release.len() + self.name.len()
    }
}

/// One file, loaded once however many workers ask.
struct Slot {
    /// Whether the index has the file.
    present: bool,
    cell: OnceLock<Loaded>,
}

struct Entry {
    sha: Option<String>,
    slot: Arc<Slot>,
    /// What the entry counts against the cache.
    bytes: usize,
    checked: Instant,
    /// Its place in the order of use, and the list it is in.
    used: u64,
    negative: bool,
}

/// Entries for files that are not there or cannot be used are kept apart from
/// parsed files: they are limited in number and are the first to go, so a
/// sender who invents release names cannot push real files out of the cache.
const MAX_NEGATIVE: usize = 4096;

#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    tick: u64,
    /// Keys by time of last use, one list per class; the first is the oldest.
    positive: BTreeMap<u64, Key>,
    negative: BTreeMap<u64, Key>,
}

impl Cache {
    fn list(&mut self, negative: bool) -> &mut BTreeMap<u64, Key> {
        if negative {
            &mut self.negative
        } else {
            &mut self.positive
        }
    }

    fn retick(&mut self, key: &Key) {
        self.tick += 1;
        let tick = self.tick;
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        let (old, negative) = (entry.used, entry.negative);
        entry.used = tick;
        let list = self.list(negative);
        list.remove(&old);
        list.insert(tick, key.clone());
    }

    /// The slot of an entry checked less than `recheck` ago.
    fn fresh(&mut self, key: &Key, now: Instant, recheck: Duration) -> Option<Arc<Slot>> {
        let entry = self.entries.get(key)?;
        if now.duration_since(entry.checked) >= recheck {
            return None;
        }
        let slot = Arc::clone(&entry.slot);
        self.retick(key);
        Some(slot)
    }

    /// The slot of an entry the index still agrees with: same file, and not
    /// one that turned out unusable (that is tried again).
    fn revalidated(&mut self, key: &Key, sha: &Option<String>, now: Instant) -> Option<Arc<Slot>> {
        let entry = self.entries.get_mut(key)?;
        if entry.sha != *sha || !entry.slot.cell.get().is_none_or(Loaded::is_usable) {
            return None;
        }
        entry.checked = now;
        let slot = Arc::clone(&entry.slot);
        self.retick(key);
        Some(slot)
    }

    fn insert(&mut self, key: &Key, sha: Option<String>, slot: Arc<Slot>, now: Instant) {
        self.remove(key);
        self.tick += 1;
        let negative = sha.is_none();
        let bytes = key.cost();
        self.bytes += bytes;
        let tick = self.tick;
        self.list(negative).insert(tick, key.clone());
        self.entries.insert(
            key.clone(),
            Entry {
                sha,
                slot,
                bytes,
                checked: now,
                used: self.tick,
                negative,
            },
        );
    }

    /// Counts what a slot turned out to hold, if its entry is still there.
    fn loaded(&mut self, key: &Key, slot: &Arc<Slot>, loaded: &Loaded) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if !Arc::ptr_eq(&entry.slot, slot) {
            return;
        }
        let extra = loaded.bytes();
        entry.bytes += extra;
        self.bytes += extra;
        if !loaded.is_usable() && !entry.negative {
            entry.negative = true;
            let used = entry.used;
            if let Some(moved) = self.positive.remove(&used) {
                self.negative.insert(used, moved);
            }
        }
    }

    fn remove(&mut self, key: &Key) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
            self.list(entry.negative).remove(&entry.used);
        }
    }

    /// The oldest key of a class other than `keep`.
    fn oldest(&self, negative: bool, keep: &Key) -> Option<Key> {
        let list = if negative {
            &self.negative
        } else {
            &self.positive
        };
        list.values().find(|key| *key != keep).cloned()
    }

    /// Drops entries, except `keep`, until the cache fits `limit` bytes and
    /// the negative ones fit [`MAX_NEGATIVE`]: the unusable first, then the
    /// least recently used parsed files.
    fn evict(&mut self, limit: usize, keep: &Key) {
        while self.negative.len() > MAX_NEGATIVE {
            match self.oldest(true, keep) {
                Some(key) => self.remove(&key),
                None => break,
            }
        }
        while self.bytes > limit {
            match self.oldest(true, keep).or_else(|| self.oldest(false, keep)) {
                Some(key) => self.remove(&key),
                None => break,
            }
        }
    }
}

pub struct Symbolicator {
    index: Arc<dyn SymbolIndex>,
    artifacts: Arc<dyn ArtifactStore>,
    config: RwLock<SymbolConfig>,
    cache: Mutex<Cache>,
    stats: Arc<SymbolStats>,
}

impl std::fmt::Debug for Symbolicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Symbolicator")
    }
}

impl Symbolicator {
    pub fn new(
        index: Arc<dyn SymbolIndex>,
        artifacts: Arc<dyn ArtifactStore>,
        config: SymbolConfig,
    ) -> Self {
        Self {
            index,
            artifacts,
            config: RwLock::new(config),
            cache: Mutex::new(Cache::default()),
            stats: Arc::new(SymbolStats::default()),
        }
    }

    /// Changes the bounds from the next batch.
    pub fn reconfigure(&self, config: SymbolConfig) {
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = config;
    }

    pub fn stats(&self) -> Arc<SymbolStats> {
        Arc::clone(&self.stats)
    }

    fn config(&self) -> SymbolConfig {
        self.config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `lines` (event rows, one per line) with the `app_error` rows
    /// symbolicated and fingerprinted. Every other line is returned as it is.
    pub fn process(&self, lines: String) -> String {
        let parts: Vec<&str> = lines.split_inclusive('\n').collect();
        let jobs: Vec<usize> = parts
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains("\"name\":\"app_error\""))
            .map(|(index, _)| index)
            .collect();
        if jobs.is_empty() {
            return lines;
        }
        let config = self.config();
        let deadline = Instant::now() + config.deadline;
        let next = AtomicUsize::new(0);
        let work = || {
            let mut done: Vec<(usize, String)> = Vec::new();
            loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(&line_index) = jobs.get(at) else {
                    break;
                };
                let line = parts[line_index];
                let late = Instant::now() >= deadline;
                let started = Instant::now();
                let changed = catch_unwind(AssertUnwindSafe(|| self.one(line, late, &config)))
                    .unwrap_or_else(|_| {
                        self.stats.panics.fetch_add(1, Ordering::Relaxed);
                        None
                    });
                self.stats.micros.fetch_add(
                    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                if let Some(changed) = changed {
                    done.push((line_index, changed));
                }
            }
            done
        };
        let workers = config.workers.clamp(1, jobs.len());
        let mut replaced: Vec<(usize, String)> = thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 1..workers {
                match thread::Builder::new().spawn_scoped(scope, work) {
                    Ok(handle) => handles.push(handle),
                    Err(_) => break,
                }
            }
            let mut all = work();
            for handle in handles {
                // `work` contains its own panics, so a join error is not expected.
                all.extend(handle.join().unwrap_or_default());
            }
            all
        });
        replaced.sort_unstable_by_key(|(index, _)| *index);
        let mut out = String::with_capacity(lines.len() + replaced.len() * 80);
        let mut replacements = replaced.into_iter().peekable();
        for (index, part) in parts.iter().enumerate() {
            match replacements.next_if(|(at, _)| *at == index) {
                Some((_, line)) => out.push_str(&line),
                None => out.push_str(part),
            }
        }
        out
    }

    /// The row with its stack translated, or `None` to keep it as it is.
    fn one(&self, line: &str, late: bool, config: &SymbolConfig) -> Option<String> {
        let value: Value = serde_json::from_str(line.trim_end()).ok()?;
        let object = value.as_object()?;
        if object.get("name")?.as_str()? != "app_error" {
            return None;
        }
        let site = u32::try_from(object.get("site_id")?.as_u64()?).ok()?;
        let text = |name: &str| object.get(name).and_then(Value::as_str).unwrap_or("");
        let (platform, release) = (text("platform"), text("app_version"));
        let mut props: Vec<(String, String)> = object
            .get("props_str")?
            .as_object()?
            .iter()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect();
        let prop = |props: &[(String, String)], name: &str| {
            props
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let raw = prop(&props, "frames").unwrap_or_default();
        let error_type = prop(&props, "type").unwrap_or_default();
        let parsed = frames::parse(&raw);

        let (state, final_frames) = if late {
            (State::Skipped, parsed)
        } else {
            self.translate(site, platform, release, parsed, config)
        };
        self.stats.count(state);
        let print = fingerprint(&error_type, &final_frames);

        if state != State::Skipped && state != State::None {
            set(&mut props, "frames", fit(&final_frames));
        }
        set(&mut props, "symbols", state.as_str().to_owned());
        set(&mut props, "fingerprint", print);
        dionaea_store::with_props_str(line, &props)
    }

    fn translate(
        &self,
        site: u32,
        platform: &str,
        release: &str,
        parsed: Vec<Frame>,
        config: &SymbolConfig,
    ) -> (State, Vec<Frame>) {
        let usable_release = is_release(release);
        let mut out: Vec<Frame> = Vec::with_capacity(parsed.len());
        let (mut translated, mut missing, mut failed) = (0usize, false, false);
        let mut consulted: Vec<Key> = Vec::new();
        for frame in parsed {
            let Some(wanted) = wanted_files(&frame, platform) else {
                out.push(frame);
                continue;
            };
            if !usable_release {
                missing = true;
                out.push(frame);
                continue;
            }
            let mut result = None;
            let mut absent = true;
            for (kind, name) in wanted {
                let key = Key {
                    site,
                    release: release.to_owned(),
                    kind,
                    name,
                };
                if !consulted.contains(&key) {
                    if consulted.len() >= MAX_FILES_PER_EVENT {
                        continue;
                    }
                    consulted.push(key.clone());
                }
                match self.file(&key, config) {
                    Loaded::Source(map) => {
                        absent = false;
                        if let Some(found) = sourcemap::symbolicate(&map, &frame) {
                            result = Some(vec![found]);
                            break;
                        }
                    }
                    Loaded::Mapping(map) => {
                        absent = false;
                        let found = proguard::symbolicate(&map, &frame);
                        if found.as_slice() != std::slice::from_ref(&frame) {
                            result = Some(found);
                            break;
                        }
                    }
                    Loaded::Failed => {
                        absent = false;
                        failed = true;
                    }
                    Loaded::Missing => {}
                }
            }
            match result {
                Some(frames) => {
                    translated += 1;
                    out.extend(frames);
                }
                None => {
                    // A file that was there and had nothing at this position
                    // is not a missing file; one that was not there is.
                    missing |= absent;
                    out.push(frame);
                }
            }
        }
        let state = if translated > 0 {
            if missing || failed {
                State::Partial
            } else {
                State::Ok
            }
        } else if failed {
            State::Failed
        } else if missing {
            State::Missing
        } else {
            State::None
        };
        (state, out)
    }

    /// The file for `key`, from the cache when it is fresh.
    fn file(&self, key: &Key, config: &SymbolConfig) -> Loaded {
        let now = Instant::now();
        let fresh = self.cache().fresh(key, now, config.recheck);
        let slot = if let Some(slot) = fresh {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            slot
        } else {
            let sha = match self
                .index
                .symbol_file(key.site, &key.release, key.kind, &key.name)
            {
                Ok(file) => file.map(|file| file.sha256),
                Err(_) => {
                    self.stats.index_errors.fetch_add(1, Ordering::Relaxed);
                    return Loaded::Failed;
                }
            };
            let mut cache = self.cache();
            let slot = cache.revalidated(key, &sha, now).unwrap_or_else(|| {
                let slot = Arc::new(Slot {
                    present: sha.is_some(),
                    cell: OnceLock::new(),
                });
                cache.insert(key, sha, Arc::clone(&slot), now);
                cache.evict(config.cache_bytes, key);
                slot
            });
            self.publish(&cache);
            slot
        };
        let mut loaded_here = false;
        let loaded = slot
            .cell
            .get_or_init(|| {
                loaded_here = true;
                self.load(key, slot.present, config)
            })
            .clone();
        if loaded_here {
            let mut cache = self.cache();
            cache.loaded(key, &slot, &loaded);
            cache.evict(config.cache_bytes, key);
            self.publish(&cache);
        }
        loaded
    }

    fn publish(&self, cache: &Cache) {
        self.stats.cache_bytes.store(cache.bytes, Ordering::Relaxed);
        self.stats
            .cache_entries
            .store(cache.entries.len(), Ordering::Relaxed);
    }

    fn load(&self, key: &Key, present: bool, config: &SymbolConfig) -> Loaded {
        if !present {
            return Loaded::Missing;
        }
        let id = ArtifactId {
            site_id: key.site,
            release: &key.release,
            kind: key.kind.as_str(),
            name: &key.name,
        };
        let bytes = match self.artifacts.get(id, MAX_FILE_BYTES as u64) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Loaded::Missing,
            Err(_) => return Loaded::Failed,
        };
        self.stats.loads.fetch_add(1, Ordering::Relaxed);
        let loaded = match key.kind {
            SymbolKind::SourceMap => SourceMap::parse(&bytes, MAX_FILE_BYTES)
                .map(|map| Loaded::Source(Arc::new(map)))
                .unwrap_or(Loaded::Failed),
            SymbolKind::Proguard => ProguardMap::parse(&bytes, MAX_FILE_BYTES)
                .map(|map| Loaded::Mapping(Arc::new(map)))
                .unwrap_or(Loaded::Failed),
        };
        // A file the cache could not hold would be read again for every event.
        if loaded.bytes() + key.cost() > config.cache_bytes {
            return Loaded::Failed;
        }
        loaded
    }
}

fn set(props: &mut Vec<(String, String)>, name: &str, value: String) {
    match props.iter_mut().find(|(key, _)| key == name) {
        Some((_, current)) => *current = value,
        None => props.push((name.to_owned(), value)),
    }
}

/// The frames as text that fits the `frames` field: frames from the end are
/// left out until it does.
fn fit(frames: &[Frame]) -> String {
    let mut count = frames.len().min(frames::MAX_FRAMES);
    loop {
        let text = frames::encode(&frames[..count]);
        if count == 0 || text.chars().count() <= MAX_FRAMES_CHARS {
            return text;
        }
        count -= 1;
    }
}

/// Whether a module names a script rather than a class.
fn is_script(module: &str) -> bool {
    module.contains(['/', ':'])
        || [
            ".js",
            ".mjs",
            ".cjs",
            ".jsx",
            ".bundle",
            ".jsbundle",
            ".hbc",
        ]
        .iter()
        .any(|ext| module.ends_with(ext))
}

/// The files that could hold the frame's original position, best first.
fn wanted_files(frame: &Frame, platform: &str) -> Option<Vec<(SymbolKind, String)>> {
    if frame.module.is_empty() {
        return None;
    }
    if is_script(&frame.module) {
        let names = map_names(&frame.module);
        return (!names.is_empty()).then(|| {
            names
                .into_iter()
                .map(|n| (SymbolKind::SourceMap, n))
                .collect()
        });
    }
    (platform == "android").then(|| vec![(SymbolKind::Proguard, MAPPING_NAME.to_owned())])
}

/// `static/js/main.js` and then `main.js`, each with `.map`: the path below
/// the host as uploaded from the build directory, or only the file name.
fn map_names(module: &str) -> Vec<String> {
    let bare = module.split(['?', '#']).next().unwrap_or("");
    let path = match bare.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or("", |(_, path)| path),
        None => bare,
    };
    let path = path.trim_start_matches('/');
    let base = path.rsplit('/').next().unwrap_or("");
    let mut names = Vec::new();
    for candidate in [path, base] {
        if candidate.is_empty() {
            continue;
        }
        let name = format!("{candidate}.map");
        if is_symbol_name(&name) && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_picks_its_candidate_map_names() {
        assert_eq!(
            map_names("https://cdn.example.com/static/js/main.3f9a.js?v=2"),
            ["static/js/main.3f9a.js.map", "main.3f9a.js.map"]
        );
        assert_eq!(
            map_names("index.android.bundle"),
            ["index.android.bundle.map"]
        );
        assert_eq!(map_names("/app/x.js"), ["app/x.js.map", "x.js.map"]);
        // Anything that is not a name an upload can have gives none.
        assert!(map_names("https://host/a b.js").is_empty());
        assert!(map_names("https://host").is_empty());
        assert!(map_names("C:\\app\\x.js").is_empty());
    }

    #[test]
    fn scripts_and_classes_are_told_apart() {
        assert!(is_script("static/js/main.js"));
        assert!(is_script("index.android.bundle"));
        assert!(is_script("https://x.test/a"));
        assert!(!is_script("com.example.Cart"));
        assert!(!is_script("a.b"));
    }

    #[test]
    fn frames_that_cannot_be_looked_up_are_left_alone() {
        let frame = |module: &str, line: u32| Frame {
            module: module.to_owned(),
            function: "f".to_owned(),
            line,
            column: 1,
            in_app: true,
        };
        assert!(wanted_files(&frame("", 3), "android").is_none());
        assert!(wanted_files(&frame("com.a.B", 0), "android").is_some());
        assert!(wanted_files(&frame("com.a.B", 3), "ios").is_none());
        assert!(wanted_files(&frame("com.a.B", 3), "android").is_some());
    }
}
