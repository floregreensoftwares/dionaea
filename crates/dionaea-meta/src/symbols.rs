//! The index of symbol files: which file of which kind was uploaded for which
//! release of a site. The bytes live in the artifact store; this index only
//! says what exists, how big it is and its SHA-256, so uploads can be listed,
//! replaced and removed with their release and a site's total can be capped.

use rusqlite::{OptionalExtension, params};

use crate::MetaError;
use crate::sqlite::{SqliteMeta, id_of, storage};

/// Longest release and file name, in characters.
pub const MAX_RELEASE_CHARS: usize = 64;
pub const MAX_SYMBOL_NAME_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    /// A JavaScript source map.
    SourceMap,
    /// An Android ProGuard or R8 mapping file.
    Proguard,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceMap => "sourcemap",
            Self::Proguard => "proguard",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sourcemap" => Some(Self::SourceMap),
            "proguard" => Some(Self::Proguard),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolFile {
    pub site_id: u32,
    pub release: String,
    pub kind: SymbolKind,
    pub name: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
    pub uploaded_at: i64,
}

/// Whether `text` is a release label: the characters the wire format allows
/// (`[A-Za-z0-9_.+/-]`), 1 to 64 of them.
pub fn is_release(text: &str) -> bool {
    (1..=MAX_RELEASE_CHARS).contains(&text.chars().count())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '/' | '-'))
}

/// Whether `text` is a symbol file name: the release characters plus `@`, 1 to
/// 256 of them, written as a relative path whose parts are never empty, `.` or
/// `..` (`static/js/main.js.map`). Names never reach a file path as they are,
/// but a name that climbs is refused all the same.
pub fn is_symbol_name(text: &str) -> bool {
    (1..=MAX_SYMBOL_NAME_CHARS).contains(&text.chars().count())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '/' | '-' | '@'))
        && text
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub trait SymbolIndex: Send + Sync {
    /// Records a file, replacing the one of the same site, release, kind and
    /// name. Returns the file it replaced, if any.
    fn record_symbol_file(&self, file: &SymbolFile) -> Result<Option<SymbolFile>, MetaError>;

    fn symbol_file(
        &self,
        site_id: u32,
        release: &str,
        kind: SymbolKind,
        name: &str,
    ) -> Result<Option<SymbolFile>, MetaError>;

    /// The files of a site, of one release when given, in name order.
    fn symbol_files(
        &self,
        site_id: u32,
        release: Option<&str>,
    ) -> Result<Vec<SymbolFile>, MetaError>;

    /// The bytes a site's files take, as recorded.
    fn symbol_bytes(&self, site_id: u32) -> Result<u64, MetaError>;

    /// Forgets one file. Returns whether it was there.
    fn remove_symbol_file(
        &self,
        site_id: u32,
        release: &str,
        kind: SymbolKind,
        name: &str,
    ) -> Result<bool, MetaError>;

    /// Forgets every file of a release and returns them, so their bytes can
    /// be removed too.
    fn remove_release_symbols(
        &self,
        site_id: u32,
        release: &str,
    ) -> Result<Vec<SymbolFile>, MetaError>;
}

fn file_of(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(i64, String, String, String, i64, String, i64)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

const COLUMNS: &str = "site_id, release, kind, name, size, sha256, uploaded_at";

fn build(raw: (i64, String, String, String, i64, String, i64)) -> Result<SymbolFile, MetaError> {
    Ok(SymbolFile {
        site_id: id_of(raw.0)?,
        release: raw.1,
        kind: SymbolKind::parse(&raw.2)
            .ok_or_else(|| MetaError::Corrupt(format!("symbol kind `{}` is unknown", raw.2)))?,
        name: raw.3,
        size: u64::try_from(raw.4).map_err(|_| MetaError::Corrupt("a negative size".to_owned()))?,
        sha256: raw.5,
        uploaded_at: raw.6,
    })
}

impl SymbolIndex for SqliteMeta {
    fn record_symbol_file(&self, file: &SymbolFile) -> Result<Option<SymbolFile>, MetaError> {
        if !is_release(&file.release) || !is_symbol_name(&file.name) {
            return Err(MetaError::Invalid(
                "a release is 1 to 64 characters of letters, digits and `_.+/-`; a file name the same plus `@` up to 256".to_owned(),
            ));
        }
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(MetaError::Invalid(
                "the SHA-256 is 64 lowercase hex characters".to_owned(),
            ));
        }
        let size = i64::try_from(file.size)
            .map_err(|_| MetaError::Invalid("the size is too large".to_owned()))?;
        self.write(|tx, at| {
            let site: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sites WHERE id = ?1)",
                    [file.site_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if !site {
                return Err(MetaError::NotFound("site"));
            }
            let previous = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM symbol_files
                         WHERE site_id = ?1 AND release = ?2 AND kind = ?3 AND name = ?4"
                    ),
                    params![file.site_id, file.release, file.kind.as_str(), file.name],
                    file_of,
                )
                .optional()
                .map_err(storage)?
                .map(build)
                .transpose()?;
            tx.execute(
                "INSERT INTO symbol_files (site_id, release, kind, name, size, sha256, uploaded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (site_id, release, kind, name)
                 DO UPDATE SET size = excluded.size, sha256 = excluded.sha256,
                               uploaded_at = excluded.uploaded_at",
                params![
                    file.site_id,
                    file.release,
                    file.kind.as_str(),
                    file.name,
                    size,
                    file.sha256,
                    at
                ],
            )
            .map_err(storage)?;
            Ok(previous)
        })
    }

    fn symbol_file(
        &self,
        site_id: u32,
        release: &str,
        kind: SymbolKind,
        name: &str,
    ) -> Result<Option<SymbolFile>, MetaError> {
        self.read(|tx| {
            tx.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM symbol_files
                     WHERE site_id = ?1 AND release = ?2 AND kind = ?3 AND name = ?4"
                ),
                params![site_id, release, kind.as_str(), name],
                file_of,
            )
            .optional()
            .map_err(storage)?
            .map(build)
            .transpose()
        })
    }

    fn symbol_files(
        &self,
        site_id: u32,
        release: Option<&str>,
    ) -> Result<Vec<SymbolFile>, MetaError> {
        self.read(|tx| {
            let mut statement = tx
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM symbol_files
                     WHERE site_id = ?1 AND (?2 IS NULL OR release = ?2)
                     ORDER BY release, kind, name"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map(params![site_id, release], file_of)
                .map_err(storage)?;
            rows.map(|row| build(row.map_err(storage)?)).collect()
        })
    }

    fn symbol_bytes(&self, site_id: u32) -> Result<u64, MetaError> {
        self.read(|tx| {
            let total: i64 = tx
                .query_row(
                    "SELECT COALESCE(SUM(size), 0) FROM symbol_files WHERE site_id = ?1",
                    [site_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            Ok(u64::try_from(total).unwrap_or(0))
        })
    }

    fn remove_symbol_file(
        &self,
        site_id: u32,
        release: &str,
        kind: SymbolKind,
        name: &str,
    ) -> Result<bool, MetaError> {
        self.write(|tx, _| {
            let removed = tx
                .execute(
                    "DELETE FROM symbol_files WHERE site_id = ?1 AND release = ?2 AND kind = ?3 AND name = ?4",
                    params![site_id, release, kind.as_str(), name],
                )
                .map_err(storage)?;
            Ok(removed > 0)
        })
    }

    fn remove_release_symbols(
        &self,
        site_id: u32,
        release: &str,
    ) -> Result<Vec<SymbolFile>, MetaError> {
        self.write(|tx, _| {
            let found = {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT {COLUMNS} FROM symbol_files WHERE site_id = ?1 AND release = ?2 ORDER BY kind, name"
                    ))
                    .map_err(storage)?;
                let rows = statement.query_map(params![site_id, release], file_of).map_err(storage)?;
                rows.map(|row| build(row.map_err(storage)?)).collect::<Result<Vec<_>, _>>()?
            };
            tx.execute(
                "DELETE FROM symbol_files WHERE site_id = ?1 AND release = ?2",
                params![site_id, release],
            )
            .map_err(storage)?;
            Ok(found)
        })
    }
}
