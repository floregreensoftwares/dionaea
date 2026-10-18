//! A reader for JavaScript source maps (version 3) that turns a position in a
//! minified file back into the original file, line, column and name.
//!
//! The file is untrusted and can be large, so the reader is built for
//! limits, not for speed of construction:
//!
//! - [`SourceMap::parse`] validates the whole `mappings` string once (every
//!   character, every number, every field count) and keeps only the text and,
//!   for each generated line, the decoder state where it starts. Memory is
//!   about the size of the file plus five numbers per line.
//! - [`SourceMap::lookup`] decodes the one generated line asked about, so a
//!   lookup costs the length of that line, not of the file. A minified bundle
//!   is often one very long line; that is the worst case, bounded by the file
//!   size.
//! - Sizes are bounded: at most [`MAX_SEGMENTS`] segments, [`MAX_LINES`]
//!   lines and [`MAX_SOURCES`] sources and names.
//!
//! Index maps (`sections`) are not supported.

use serde::Deserialize;

use crate::frames::Frame;

/// Most mapping segments one map may hold.
pub const MAX_SEGMENTS: usize = 8_000_000;
/// Most generated lines one map may have.
pub const MAX_LINES: usize = 1_000_000;
/// Most entries in `sources` and in `names`.
pub const MAX_SOURCES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMapError {
    /// Not JSON, or not a source map object.
    Invalid(&'static str),
    /// A version other than 3, or an index map.
    Unsupported(&'static str),
    /// Larger than the limits allow.
    TooLarge,
    /// The `mappings` text is malformed.
    BadMappings,
}

impl std::fmt::Display for SourceMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "not a source map: {why}"),
            Self::Unsupported(what) => write!(f, "unsupported source map: {what}"),
            Self::TooLarge => f.write_str("the source map is larger than the limits allow"),
            Self::BadMappings => f.write_str("the source map has malformed mappings"),
        }
    }
}

impl std::error::Error for SourceMapError {}

#[derive(Deserialize)]
struct Raw {
    version: Option<u32>,
    #[serde(default)]
    sources: Vec<Option<String>>,
    #[serde(default)]
    names: Vec<Option<String>>,
    #[serde(default, rename = "sourcesContent")]
    sources_content: Vec<Option<String>>,
    mappings: Option<String>,
    #[serde(rename = "sourceRoot")]
    source_root: Option<String>,
    sections: Option<serde::de::IgnoredAny>,
}

/// The decoder state at the start of a generated line.
#[derive(Debug, Clone, Copy, Default)]
struct LineStart {
    offset: usize,
    source: i64,
    line: i64,
    column: i64,
    name: i64,
}

/// A position in an original file. Line and column start at 1, as in stack
/// traces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original<'a> {
    /// Which entry of the map's sources, for [`SourceMap::function_name`].
    pub source_index: usize,
    pub source: &'a str,
    pub line: u32,
    pub column: u32,
    pub name: Option<&'a str>,
}

#[derive(Debug)]
pub struct SourceMap {
    sources: Vec<String>,
    contents: Vec<Option<String>>,
    names: Vec<String>,
    mappings: String,
    lines: Vec<LineStart>,
}

fn digit(byte: u8) -> Option<u32> {
    Some(match byte {
        b'A'..=b'Z' => u32::from(byte - b'A'),
        b'a'..=b'z' => u32::from(byte - b'a') + 26,
        b'0'..=b'9' => u32::from(byte - b'0') + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

/// Reads one base64 VLQ number from `bytes` at `*at`.
fn vlq(bytes: &[u8], at: &mut usize) -> Option<i64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let d = digit(*bytes.get(*at)?)?;
        *at += 1;
        // 32 bits of value are more than any real map needs.
        if shift > 30 {
            return None;
        }
        result |= u64::from(d & 31) << shift;
        shift += 5;
        if d & 32 == 0 {
            break;
        }
    }
    let magnitude = i64::try_from(result >> 1).ok()?;
    Some(if result & 1 == 1 {
        -magnitude
    } else {
        magnitude
    })
}

/// One decoded segment: generated column and, when it maps anything, the
/// original position.
#[derive(Debug, Clone, Copy)]
struct Segment {
    generated_column: i64,
    original: Option<(i64, i64, i64, Option<i64>)>,
}

/// Decodes one line of `mappings` starting at `state`; calls `each` for every
/// segment until it returns false, and returns where it stopped (the end of the
/// line when it never did), or `None` if the text is malformed. `state` is updated to
/// where the line ends (its source, line, column and name).
fn decode_line(
    bytes: &[u8],
    state: &mut LineStart,
    mut each: impl FnMut(Segment) -> bool,
) -> Option<usize> {
    let mut at = state.offset;
    let mut generated_column = 0i64;
    while at < bytes.len() && bytes[at] != b';' {
        if bytes[at] == b',' {
            at += 1;
            continue;
        }
        generated_column = generated_column.checked_add(vlq(bytes, &mut at)?)?;
        if generated_column < 0 {
            return None;
        }
        let mut original = None;
        if at < bytes.len() && !matches!(bytes[at], b',' | b';') {
            state.source = state.source.checked_add(vlq(bytes, &mut at)?)?;
            state.line = state.line.checked_add(vlq(bytes, &mut at)?)?;
            state.column = state.column.checked_add(vlq(bytes, &mut at)?)?;
            let mut name = None;
            if at < bytes.len() && !matches!(bytes[at], b',' | b';') {
                state.name = state.name.checked_add(vlq(bytes, &mut at)?)?;
                name = Some(state.name);
            }
            if state.source < 0 || state.line < 0 || state.column < 0 {
                return None;
            }
            // A fifth field is the last: anything more is malformed.
            if at < bytes.len() && !matches!(bytes[at], b',' | b';') {
                return None;
            }
            original = Some((state.source, state.line, state.column, name));
        }
        if !each(Segment {
            generated_column,
            original,
        }) {
            break;
        }
    }
    // When `each` stopped early the rest of the line is not read: a lookup
    // costs up to its position, not the length of the line.
    Some(at)
}

impl SourceMap {
    /// Parses and validates a source map. `max_bytes` bounds the input.
    pub fn parse(bytes: &[u8], max_bytes: usize) -> Result<Self, SourceMapError> {
        if bytes.len() > max_bytes {
            return Err(SourceMapError::TooLarge);
        }
        let raw: Raw = serde_json::from_slice(bytes)
            .map_err(|_| SourceMapError::Invalid("not a JSON object"))?;
        if raw.sections.is_some() {
            return Err(SourceMapError::Unsupported("index maps"));
        }
        if raw.version != Some(3) {
            return Err(SourceMapError::Unsupported("a version other than 3"));
        }
        let mappings = raw.mappings.ok_or(SourceMapError::Invalid("no mappings"))?;
        if raw.sources.len() > MAX_SOURCES || raw.names.len() > MAX_SOURCES {
            return Err(SourceMapError::TooLarge);
        }
        let root = raw.source_root.unwrap_or_default();
        // A long root joined to many short sources would take memory in the
        // product of the two; the joined names may not exceed the input bound.
        let mut joined = 0usize;
        let mut sources: Vec<String> = Vec::with_capacity(raw.sources.len());
        for source in raw.sources {
            let name = join_root(&root, &source.unwrap_or_default());
            joined = joined.saturating_add(name.len());
            if joined > max_bytes {
                return Err(SourceMapError::TooLarge);
            }
            sources.push(name);
        }
        let names: Vec<String> = raw
            .names
            .into_iter()
            .map(Option::unwrap_or_default)
            .collect();
        let mut contents = raw.sources_content;
        contents.truncate(sources.len());
        contents.resize(sources.len(), None);

        let bytes = mappings.as_bytes();
        let mut lines = vec![LineStart::default()];
        let mut state = LineStart::default();
        let mut segments = 0usize;
        let mut out_of_range = false;
        loop {
            let at = decode_line(bytes, &mut state, |segment| {
                segments += 1;
                if let Some((source, _, _, name)) = segment.original {
                    // An index outside the arrays is malformed.
                    let source_ok = usize::try_from(source).is_ok_and(|i| i < sources.len());
                    let name_ok =
                        name.is_none_or(|n| usize::try_from(n).is_ok_and(|i| i < names.len()));
                    out_of_range |= !source_ok || !name_ok;
                }
                !out_of_range && segments <= MAX_SEGMENTS
            })
            .ok_or(SourceMapError::BadMappings)?;
            if out_of_range {
                return Err(SourceMapError::BadMappings);
            }
            if segments > MAX_SEGMENTS {
                return Err(SourceMapError::TooLarge);
            }
            if at >= bytes.len() {
                break;
            }
            // `at` is a ';': the next line starts after it.
            state.offset = at + 1;
            if lines.len() >= MAX_LINES {
                return Err(SourceMapError::TooLarge);
            }
            lines.push(state);
        }
        Ok(Self {
            sources,
            contents,
            names,
            mappings,
            lines,
        })
    }

    /// The original position for a generated one. `line` and `column` start at
    /// 1, as in a stack trace. `None` when nothing maps there.
    pub fn lookup(&self, line: u32, column: u32) -> Option<Original<'_>> {
        let line_index = usize::try_from(line.checked_sub(1)?).ok()?;
        let wanted = i64::from(column.checked_sub(1)?);
        let mut state = *self.lines.get(line_index)?;
        let mut best: Option<Segment> = None;
        decode_line(self.mappings.as_bytes(), &mut state, |segment| {
            if segment.generated_column <= wanted {
                best = Some(segment);
                true
            } else {
                false
            }
        })?;
        let (source, orig_line, orig_column, name) = best?.original?;
        let source_index = usize::try_from(source).ok()?;
        Some(Original {
            source_index,
            source: self.sources.get(source_index)?.as_str(),
            line: u32::try_from(orig_line).ok()?.checked_add(1)?,
            column: u32::try_from(orig_column).ok()?.checked_add(1)?,
            name: name
                .and_then(|n| usize::try_from(n).ok())
                .and_then(|n| self.names.get(n))
                .map(String::as_str),
        })
    }

    /// Whether the map carries the text of the source `original` is in.
    pub fn has_source_text(&self, original: &Original<'_>) -> bool {
        self.contents
            .get(original.source_index)
            .is_some_and(Option::is_some)
    }

    /// The name of the function around an original position, read from the
    /// source the map carries; `None` when the map has no source text or no
    /// named function is around (see [`crate::funcname`]).
    pub fn function_name(&self, original: &Original<'_>) -> Option<String> {
        let text = self.contents.get(original.source_index)?.as_deref()?;
        crate::funcname::enclosing_function(text, original.line, original.column)
    }

    /// Bytes the map holds, for sizing a cache.
    pub fn approx_bytes(&self) -> usize {
        self.mappings.len()
            + self
                .contents
                .iter()
                .flatten()
                .map(String::len)
                .sum::<usize>()
            + self.lines.len() * std::mem::size_of::<LineStart>()
            + self.sources.iter().map(String::len).sum::<usize>()
            + self.names.iter().map(String::len).sum::<usize>()
    }
}

/// A source path as a stable module name: without the bundler's scheme
/// (`webpack:///`, `file://`) and without leading `./` and `../`.
pub fn normalize_source(source: &str) -> String {
    let mut rest = source;
    for scheme in ["webpack://", "turbopack://", "file://", "metro://"] {
        if let Some(after) = rest.strip_prefix(scheme) {
            // `webpack://app/./src/a.js`: the part before the first `/` is the
            // bundle's name.
            rest = after.split_once('/').map_or("", |(_, path)| path);
            break;
        }
    }
    loop {
        if let Some(after) = rest.strip_prefix("./") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix("../") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix('/') {
            rest = after;
        } else {
            break;
        }
    }
    rest.split(['?', '#']).next().unwrap_or("").to_owned()
}

/// The frame a minified JavaScript frame stands for, or `None` when the map
/// has nothing at that position. The function is the one read from the
/// original source; `<anonymous>` when the map has the source and no named
/// function is around; else the one the stack had.
pub fn symbolicate(map: &SourceMap, frame: &Frame) -> Option<Frame> {
    let original = map.lookup(frame.line, frame.column)?;
    let module = normalize_source(original.source);
    // The name stored in the map at a position is the identifier written
    // there (a callee at a call site), not the function the frame is in, so it
    // is not used. Where the map carries the source text and no named function is around
    // (the top level, an anonymous callback), the function is `<anonymous>`
    // and not the minified name of the stack, which changes with every build.
    let function = map.function_name(&original).unwrap_or_else(|| {
        if map.has_source_text(&original) {
            "<anonymous>".to_owned()
        } else {
            frame.function.clone()
        }
    });
    Some(Frame {
        in_app: frame.in_app && !module.contains("node_modules/"),
        module,
        function,
        line: original.line,
        column: original.column,
    })
}

fn join_root(root: &str, source: &str) -> String {
    if root.is_empty() || source.starts_with('/') || source.contains("://") {
        source.to_owned()
    } else if root.ends_with('/') {
        format!("{root}{source}")
    } else {
        format!("{root}/{source}")
    }
}

/// Encodes numbers as a VLQ, for tests and fixtures.
#[cfg(test)]
pub(crate) fn encode_vlq(value: i64) -> String {
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut n = (value.unsigned_abs() << 1) | u64::from(value < 0);
    let mut out = String::new();
    loop {
        let mut digit = (n & 31) as usize;
        n >>= 5;
        if n > 0 {
            digit |= 32;
        }
        out.push(char::from(BASE64[digit]));
        if n == 0 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(mappings: &str) -> Vec<u8> {
        format!(
            r#"{{"version":3,"sources":["src/a.js","src/b.js"],"names":["alpha","beta"],"mappings":"{mappings}"}}"#
        )
        .into_bytes()
    }

    fn seg(fields: &[i64]) -> String {
        fields.iter().map(|f| encode_vlq(*f)).collect()
    }

    #[test]
    fn vlq_round_trips_and_refuses_bad_input() {
        for value in [
            0,
            1,
            -1,
            15,
            16,
            -16,
            31,
            32,
            1000,
            -1000,
            1 << 30,
            -(1 << 30),
        ] {
            let text = encode_vlq(value);
            let mut at = 0;
            assert_eq!(vlq(text.as_bytes(), &mut at), Some(value), "{value}");
            assert_eq!(at, text.len());
        }
        assert_eq!(vlq(b"", &mut 0), None);
        assert_eq!(vlq(b"!", &mut 0), None);
        assert_eq!(vlq(b"g", &mut 0), None, "ends in a continuation");
        // Too many continuation digits for a 32-bit value.
        assert_eq!(vlq(b"gggggggggggg", &mut 0), None);
    }

    #[test]
    fn a_position_maps_back_with_its_name() {
        // Line 1: column 0 -> a.js 0:0; column 10 -> a.js 4:2 named alpha.
        // Line 2: column 5 -> b.js (source +1) line 10 column 3 named beta.
        let m = format!(
            "{},{};{}",
            seg(&[0, 0, 0, 0]),
            seg(&[10, 0, 4, 2, 0]),
            seg(&[5, 1, 6, 1, 1])
        );
        let map = SourceMap::parse(&map(&m), 1 << 20).expect("parse");
        let first = map.lookup(1, 1).expect("a mapping");
        assert_eq!(
            (first.source, first.line, first.column, first.name),
            ("src/a.js", 1, 1, None)
        );
        let named = map.lookup(1, 11).expect("a mapping");
        assert_eq!(
            (named.source, named.line, named.column, named.name),
            ("src/a.js", 5, 3, Some("alpha"))
        );
        // Between segments the earlier one applies.
        assert_eq!(map.lookup(1, 8).expect("a mapping").line, 1);
        assert_eq!(map.lookup(1, 500).expect("a mapping").line, 5);
        let second = map.lookup(2, 6).expect("a mapping");
        assert_eq!(
            (second.source, second.line, second.column, second.name),
            ("src/b.js", 11, 4, Some("beta"))
        );
    }

    #[test]
    fn positions_before_the_first_segment_or_outside_the_map_map_nowhere() {
        let m = seg(&[4, 0, 0, 0]);
        let map = SourceMap::parse(&map(&m), 1 << 20).expect("parse");
        assert!(map.lookup(1, 1).is_none(), "before the first segment");
        assert!(map.lookup(1, 5).is_some());
        assert!(map.lookup(2, 1).is_none(), "no such line");
        assert!(
            map.lookup(0, 1).is_none() && map.lookup(1, 0).is_none(),
            "lines and columns start at 1"
        );
    }

    #[test]
    fn empty_lines_and_one_field_segments_are_allowed() {
        let m = format!(";;{};{}", seg(&[0, 0, 1, 0]), seg(&[3]));
        let map = SourceMap::parse(&map(&m), 1 << 20).expect("parse");
        assert!(map.lookup(1, 1).is_none());
        assert_eq!(map.lookup(3, 1).expect("a mapping").line, 2);
        assert!(
            map.lookup(4, 10).is_none(),
            "a segment without an original position"
        );
    }

    #[test]
    fn the_state_carries_across_lines() {
        // Line 1 ends at original line 5; line 2's first segment is relative to it.
        let m = format!("{};{}", seg(&[0, 0, 5, 0]), seg(&[0, 0, 2, 0]));
        let map = SourceMap::parse(&map(&m), 1 << 20).expect("parse");
        assert_eq!(map.lookup(1, 1).expect("a mapping").line, 6);
        assert_eq!(map.lookup(2, 1).expect("a mapping").line, 8);
    }

    #[test]
    fn malformed_maps_are_refused() {
        let bad_mappings = [
            "!!!",
            "g",
            &seg(&[0, 5, 0, 0]),    // source index past the sources
            &seg(&[0, 0, 0, 0, 5]), // name index past the names
            &format!("{}A", seg(&[0, 0, 0, 0, 0])), // a sixth field
            &seg(&[-1]),            // negative generated column
            &seg(&[0, -1, 0, 0]),   // negative source
        ];
        for m in bad_mappings {
            assert_eq!(
                SourceMap::parse(&map(m), 1 << 20).unwrap_err(),
                SourceMapError::BadMappings,
                "{m}"
            );
        }
        for (text, expected) in [
            ("not json", SourceMapError::Invalid("not a JSON object")),
            (
                r#"{"version":2,"mappings":""}"#,
                SourceMapError::Unsupported("a version other than 3"),
            ),
            (r#"{"version":3}"#, SourceMapError::Invalid("no mappings")),
            (
                r#"{"version":3,"sections":[],"mappings":""}"#,
                SourceMapError::Unsupported("index maps"),
            ),
        ] {
            assert_eq!(
                SourceMap::parse(text.as_bytes(), 1 << 20).unwrap_err(),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn size_limits_are_enforced() {
        assert_eq!(
            SourceMap::parse(&map(""), 10).unwrap_err(),
            SourceMapError::TooLarge
        );
        let many_lines = ";".repeat(MAX_LINES + 1);
        assert_eq!(
            SourceMap::parse(&map(&many_lines), 1 << 30).unwrap_err(),
            SourceMapError::TooLarge
        );
    }

    #[test]
    fn a_long_root_joined_to_many_sources_is_bounded_by_the_input_limit() {
        let sources = vec!["\"a.js\""; 2000].join(",");
        let text = format!(
            "{{\"version\":3,\"sourceRoot\":\"{}\",\"sources\":[{sources}],\"mappings\":\"AAAA\"}}",
            "r".repeat(1000)
        );
        // About 14 KB in, about 2 MB once every source carries the root.
        assert_eq!(
            SourceMap::parse(text.as_bytes(), 20_000).unwrap_err(),
            SourceMapError::TooLarge
        );
        assert!(SourceMap::parse(text.as_bytes(), 4 << 20).is_ok());
    }

    #[test]
    fn sources_become_stable_module_names() {
        for (source, expected) in [
            ("webpack:///./src/a.js", "src/a.js"),
            ("webpack://app/./src/a.js?x", "src/a.js"),
            ("webpack:///webpack/bootstrap", "webpack/bootstrap"),
            ("../src/util.js", "src/util.js"),
            ("/abs/path/a.js", "abs/path/a.js"),
            ("file:///home/u/a.js", "home/u/a.js"),
            ("node_modules/react/index.js", "node_modules/react/index.js"),
            ("", ""),
        ] {
            assert_eq!(normalize_source(source), expected, "{source}");
        }
    }

    #[test]
    fn a_frame_in_a_library_is_not_the_apps_after_mapping() {
        let text = r#"{"version":3,"sources":["node_modules/lib/x.js","src/a.js"],"names":[],"mappings":"AAAA,KCAA"}"#;
        let map = SourceMap::parse(text.as_bytes(), 1 << 20).expect("parse");
        let frame = |column| Frame {
            module: "bundle.js".to_owned(),
            function: "q".to_owned(),
            line: 1,
            column,
            in_app: true,
        };
        let lib = symbolicate(&map, &frame(1)).expect("mapped");
        assert_eq!(
            (lib.module.as_str(), lib.in_app, lib.function.as_str()),
            ("node_modules/lib/x.js", false, "q")
        );
        let own = symbolicate(&map, &frame(6)).expect("mapped");
        assert_eq!((own.module.as_str(), own.in_app), ("src/a.js", true));
        assert!(
            symbolicate(
                &map,
                &Frame {
                    line: 9,
                    ..frame(1)
                }
            )
            .is_none()
        );
    }

    #[test]
    fn the_source_root_is_joined() {
        let text = r#"{"version":3,"sourceRoot":"webpack:///","sources":["a.js","/abs.js","http://x/y.js"],"names":[],"mappings":"AAAA"}"#;
        let map = SourceMap::parse(text.as_bytes(), 1 << 20).expect("parse");
        assert_eq!(
            map.lookup(1, 1).expect("a mapping").source,
            "webpack:///a.js"
        );
        assert_eq!(join_root("r", "/abs.js"), "/abs.js");
        assert_eq!(join_root("r", "http://x/y.js"), "http://x/y.js");
        assert_eq!(join_root("r/", "a.js"), "r/a.js");
    }

    #[test]
    fn a_single_very_long_line_is_decoded_for_one_lookup_only() {
        // A minified bundle: one line with 200,000 segments, one every 2 columns.
        let mut m = String::new();
        for i in 0..200_000 {
            if i > 0 {
                m.push(',');
            }
            m.push_str(&seg(&[2, 0, 0, 1]));
        }
        let map = SourceMap::parse(&map(&m), 1 << 30).expect("parse");
        assert!(map.lookup(1, 2).is_none(), "before the first segment");
        assert_eq!(map.lookup(1, 3).expect("a mapping").column, 2);
        let far = map.lookup(1, 400_001).expect("a mapping");
        assert_eq!(far.column, 200_001);
        assert_eq!(map.lines.len(), 1);
    }
}
