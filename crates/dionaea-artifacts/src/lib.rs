//! Storage for the files an app uploads for a release (source maps, mapping
//! files), behind the [`ArtifactStore`] trait so another place (an object
//! store) can replace the local directory without touching the callers.
//!
//! [`LocalArtifacts`] keeps one file per artifact under a directory. **No
//! caller text ever becomes part of a path**: the release and the file name
//! are hashed, so `../`, absolute paths, device names and case tricks have
//! nothing to act on. A write goes to a temporary file in the same directory,
//! is flushed to disk and then renamed over the target, so a reader sees the
//! old file or the new one, never half of one, and a crash leaves at most a
//! temporary file that the next write of that artifact does not trip over.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

/// Which artifact: a site, a release, a kind (`sourcemap`, `proguard`) and a
/// file name. The strings are only ever hashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactId<'a> {
    pub site_id: u32,
    pub release: &'a str,
    pub kind: &'a str,
    pub name: &'a str,
}

/// What was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub size: u64,
    /// SHA-256 in lowercase hex.
    pub sha256: String,
}

#[derive(Debug)]
pub enum ArtifactError {
    /// The artifact is larger than the limit given.
    TooLarge,
    /// The directory or a file could not be used.
    Io(io::Error),
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("the artifact is larger than allowed"),
            Self::Io(error) => write!(f, "artifact storage failed: {error}"),
        }
    }
}

impl std::error::Error for ArtifactError {}

impl From<io::Error> for ArtifactError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub trait ArtifactStore: Send + Sync {
    /// Stores the bytes, replacing an artifact with the same id.
    fn put(&self, id: ArtifactId<'_>, bytes: &[u8]) -> Result<Stored, ArtifactError>;

    /// The bytes, or `None` when there is no such artifact. Refuses to read
    /// more than `max_bytes`.
    fn get(&self, id: ArtifactId<'_>, max_bytes: u64) -> Result<Option<Vec<u8>>, ArtifactError>;

    /// Removes one artifact. Returns whether it was there.
    fn delete(&self, id: ArtifactId<'_>) -> Result<bool, ArtifactError>;

    /// Removes every artifact of a release.
    fn delete_release(&self, site_id: u32, release: &str) -> Result<(), ArtifactError>;
}

/// A directory of artifacts.
#[derive(Debug)]
pub struct LocalArtifacts {
    root: PathBuf,
    counter: AtomicU64,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The first 32 hex characters of the SHA-256 of `text`: enough that two
/// different names do not share a path, and nothing of the name in it.
fn path_part(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes())[..16])
}

/// A kind is a short lowercase word; it becomes a directory name.
fn kind_part(kind: &str) -> io::Result<&str> {
    if (1..=16).contains(&kind.len()) && kind.bytes().all(|b| b.is_ascii_lowercase()) {
        Ok(kind)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an artifact kind is 1 to 16 lowercase letters",
        ))
    }
}

impl LocalArtifacts {
    /// Uses `root`, creating it if needed.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            counter: AtomicU64::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn release_dir(&self, site_id: u32, release: &str) -> PathBuf {
        self.root.join(site_id.to_string()).join(path_part(release))
    }

    fn file_path(&self, id: ArtifactId<'_>) -> io::Result<PathBuf> {
        Ok(self
            .release_dir(id.site_id, id.release)
            .join(kind_part(id.kind)?)
            .join(path_part(id.name)))
    }

    /// A name no other writer uses, even another server on the same directory
    /// (process ids repeat between containers): 64 random bits and a count.
    fn temp_name(&self) -> io::Result<String> {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        Ok(format!(
            ".tmp-{}-{}",
            hex(&random),
            self.counter.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

impl ArtifactStore for LocalArtifacts {
    fn put(&self, id: ArtifactId<'_>, bytes: &[u8]) -> Result<Stored, ArtifactError> {
        let path = self.file_path(id)?;
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "an artifact has a directory")
        })?;
        fs::create_dir_all(dir)?;
        let temp = dir.join(self.temp_name()?);
        let written = (|| -> io::Result<()> {
            let mut file = File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(Stored {
            size: bytes.len() as u64,
            sha256: hex(&Sha256::digest(bytes)),
        })
    }

    fn get(&self, id: ArtifactId<'_>, max_bytes: u64) -> Result<Option<Vec<u8>>, ArtifactError> {
        let path = self.file_path(id)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        // One byte more than allowed, to tell a file at the limit from past it.
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(ArtifactError::TooLarge);
        }
        Ok(Some(bytes))
    }

    fn delete(&self, id: ArtifactId<'_>) -> Result<bool, ArtifactError> {
        match fs::remove_file(self.file_path(id)?) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn delete_release(&self, site_id: u32, release: &str) -> Result<(), ArtifactError> {
        match fs::remove_dir_all(self.release_dir(site_id, release)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dionaea-artifacts-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn id<'a>(site_id: u32, release: &'a str, name: &'a str) -> ArtifactId<'a> {
        ArtifactId {
            site_id,
            release,
            kind: "sourcemap",
            name,
        }
    }

    #[test]
    fn a_file_is_stored_read_back_replaced_and_deleted() {
        let root = dir("round");
        let store = LocalArtifacts::open(&root).expect("open");
        let key = id(1, "1.4.2+57", "app.js.map");
        assert_eq!(store.get(key, 1000).expect("get"), None);

        let stored = store.put(key, b"first").expect("put");
        assert_eq!(stored.size, 5);
        assert_eq!(stored.sha256, hex(&Sha256::digest(b"first")));
        assert_eq!(store.get(key, 1000).expect("get"), Some(b"first".to_vec()));

        store.put(key, b"second one").expect("replace");
        assert_eq!(
            store.get(key, 1000).expect("get"),
            Some(b"second one".to_vec())
        );

        assert!(store.delete(key).expect("delete"));
        assert!(!store.delete(key).expect("delete again"));
        assert_eq!(store.get(key, 1000).expect("get"), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn no_caller_text_reaches_a_path() {
        let root = dir("paths");
        let store = LocalArtifacts::open(&root).expect("open");
        let hostile = [
            "../../../etc/passwd",
            "..\\..\\windows\\system32",
            "/absolute/path",
            "C:\\temp\\x",
            "CON",
            "a/b/../../c",
            "NUL.map",
            "\u{0}",
            "name with spaces and \u{e9}",
        ];
        for name in hostile {
            store.put(id(1, name, name), name.as_bytes()).expect("put");
        }
        // Everything is inside the root, under hashed names.
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).expect("read dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    walk(&path, found);
                } else {
                    found.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&root, &mut files);
        assert_eq!(files.len(), hostile.len());
        for file in files {
            let relative = file.strip_prefix(&root).expect("inside the root");
            for part in relative.components() {
                let part = part.as_os_str().to_string_lossy();
                assert!(
                    part.chars()
                        .all(|c| c.is_ascii_hexdigit() || c.is_ascii_lowercase()),
                    "{part}"
                );
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_bad_kind_is_refused_and_ids_differing_in_one_part_do_not_collide() {
        let root = dir("kinds");
        let store = LocalArtifacts::open(&root).expect("open");
        for kind in ["", "../x", "Source", "a b", &"k".repeat(17)] {
            let bad = ArtifactId {
                kind,
                ..id(1, "r", "n")
            };
            assert!(store.put(bad, b"x").is_err(), "{kind:?}");
        }
        let base = id(1, "r", "n");
        store.put(base, b"base").expect("put");
        for other in [
            id(2, "r", "n"),
            id(1, "s", "n"),
            id(1, "r", "m"),
            ArtifactId {
                kind: "proguard",
                ..base
            },
        ] {
            assert_eq!(store.get(other, 100).expect("get"), None, "{other:?}");
        }
        assert_eq!(store.get(base, 100).expect("get"), Some(b"base".to_vec()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_read_stops_at_the_limit() {
        let root = dir("limit");
        let store = LocalArtifacts::open(&root).expect("open");
        let key = id(1, "r", "n");
        store.put(key, &[7u8; 100]).expect("put");
        assert!(store.get(key, 100).expect("get").is_some());
        assert!(matches!(store.get(key, 99), Err(ArtifactError::TooLarge)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_release_is_removed_whole_and_a_neighbour_stays() {
        let root = dir("release");
        let store = LocalArtifacts::open(&root).expect("open");
        store.put(id(1, "1.0", "a"), b"a").expect("put");
        store.put(id(1, "1.0", "b"), b"b").expect("put");
        store.put(id(1, "2.0", "a"), b"c").expect("put");
        store.put(id(2, "1.0", "a"), b"d").expect("put");
        store.delete_release(1, "1.0").expect("delete");
        store.delete_release(1, "1.0").expect("again");
        assert_eq!(store.get(id(1, "1.0", "a"), 10).expect("get"), None);
        assert_eq!(store.get(id(1, "1.0", "b"), 10).expect("get"), None);
        assert!(store.get(id(1, "2.0", "a"), 10).expect("get").is_some());
        assert!(store.get(id(2, "1.0", "a"), 10).expect("get").is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn no_temporary_file_is_left_after_writes() {
        let root = dir("temp");
        let store = LocalArtifacts::open(&root).expect("open");
        for n in 0..5 {
            store
                .put(id(1, "r", "n"), format!("v{n}").as_bytes())
                .expect("put");
        }
        fn count_temp(dir: &Path) -> usize {
            fs::read_dir(dir)
                .expect("read dir")
                .map(|e| e.expect("entry").path())
                .map(|p| {
                    if p.is_dir() {
                        count_temp(&p)
                    } else {
                        usize::from(
                            p.file_name()
                                .is_some_and(|n| n.to_string_lossy().starts_with(".tmp")),
                        )
                    }
                })
                .sum()
        }
        assert_eq!(count_temp(&root), 0);
        let _ = fs::remove_dir_all(root);
    }
}
