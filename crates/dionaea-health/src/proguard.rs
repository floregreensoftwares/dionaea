//! A reader for the mapping files ProGuard and R8 write (`mapping.txt`), which
//! turns an obfuscated class, method and line back into the original ones.
//!
//! ```text
//! com.example.Cart -> a.a:
//!     int count -> a
//!     12:15:void checkout(int):40:43 -> b
//!     16:16:void com.example.Util.round(double):7:7 -> b
//! ```
//!
//! A class line gives the original class and the obfuscated name. A method
//! line gives an optional range of obfuscated lines, the return type, the
//! method (prefixed by its class when the code was inlined from another one),
//! an optional range of original lines, and the obfuscated name. Field lines
//! and comments are skipped.
//!
//! The file is untrusted: input size, line length and the numbers of classes
//! and methods are bounded, and a line that does not parse is skipped rather
//! than trusted.
//!
//! When R8 inlines a method, several lines share one obfuscated range; they
//! are returned in file order, which is innermost call first, the order of a
//! stack. Whether a line is inlined or the method itself is not distinguished
//! beyond that order.

use std::collections::HashMap;

use crate::frames::Frame;

/// Most classes, and most method lines, one mapping may hold.
pub const MAX_CLASSES: usize = 1_000_000;
pub const MAX_METHODS: usize = 4_000_000;
/// Longest line read, in bytes; a longer one is skipped.
const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProguardError {
    TooLarge,
    /// Not UTF-8 text.
    NotText,
    /// No class mapping in the file.
    Empty,
}

impl std::fmt::Display for ProguardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("the mapping file is larger than the limits allow"),
            Self::NotText => f.write_str("the mapping file is not UTF-8 text"),
            Self::Empty => f.write_str("the mapping file maps no class"),
        }
    }
}

impl std::error::Error for ProguardError {}

#[derive(Debug, Clone)]
struct Method {
    obf_range: Option<(u32, u32)>,
    /// The class the method came from, when it was inlined from another.
    class: Option<String>,
    name: String,
    original_range: Option<(u32, u32)>,
}

#[derive(Debug, Default)]
struct Class {
    original: String,
    methods: HashMap<String, Vec<Method>>,
}

#[derive(Debug)]
pub struct ProguardMap {
    classes: HashMap<String, Class>,
    methods: usize,
}

/// One original frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retraced {
    pub class: String,
    pub method: String,
    /// 0 when the mapping has no line for the frame.
    pub line: u32,
}

fn number(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 9 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `a:b` or `a` (then `a:a`) as a range.
fn range(first: &str, second: Option<&str>) -> Option<(u32, u32)> {
    let start = number(first)?;
    let end = match second {
        Some(second) => number(second)?,
        None => start,
    };
    (start <= end).then_some((start, end))
}

/// Reads `[a:b:]RETURN [CLASS.]NAME(ARGS)[:c[:d]] -> OBF` into the obfuscated
/// name and the method; `None` for anything else (fields, comments).
fn method_line(line: &str) -> Option<(String, Method)> {
    let (left, obf) = line.split_once(" -> ")?;
    let obf = obf.trim();
    if obf.is_empty() || obf.contains(' ') {
        return None;
    }
    let open = left.find('(')?;
    let close = left.rfind(')')?;
    if close < open {
        return None;
    }
    // Before the parameters: `[a:b:]RETURN NAME`.
    let head = &left[..open];
    let (prefix, qualified) = head.rsplit_once(' ')?;
    let mut obf_range = None;
    let mut parts = prefix.splitn(3, ':');
    let first = parts.next()?;
    if let (Some(second), Some(_)) = (parts.next(), parts.next()) {
        obf_range = range(first, Some(second));
        obf_range?;
    }
    // After the parameters: `:c` or `:c:d`.
    let tail = &left[close + 1..];
    let original_range = match tail.strip_prefix(':') {
        None if tail.is_empty() => None,
        None => return None,
        Some(rest) => {
            let mut numbers = rest.splitn(2, ':');
            let first = numbers.next()?;
            Some(range(first, numbers.next())?)
        }
    };
    let (class, name) = match qualified.rsplit_once('.') {
        Some((class, name)) => (Some(class.to_owned()), name),
        None => (None, qualified),
    };
    if name.is_empty() {
        return None;
    }
    Some((
        obf.to_owned(),
        Method {
            obf_range,
            class,
            name: name.to_owned(),
            original_range,
        },
    ))
}

impl ProguardMap {
    /// Parses a mapping file. `max_bytes` bounds the input.
    pub fn parse(bytes: &[u8], max_bytes: usize) -> Result<Self, ProguardError> {
        if bytes.len() > max_bytes {
            return Err(ProguardError::TooLarge);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| ProguardError::NotText)?;
        let mut classes: HashMap<String, Class> = HashMap::new();
        let mut current: Option<String> = None;
        let mut methods = 0usize;
        for line in text.lines() {
            if line.len() > MAX_LINE || line.trim().is_empty() || line.trim_start().starts_with('#')
            {
                continue;
            }
            if !line.starts_with(char::is_whitespace) {
                // A class line: `ORIGINAL -> OBFUSCATED:`.
                current = None;
                let Some((original, obf)) = line.split_once(" -> ") else {
                    continue;
                };
                let Some(obf) = obf.trim_end().strip_suffix(':') else {
                    continue;
                };
                if original.is_empty() || obf.is_empty() || original.contains(' ') {
                    continue;
                }
                if classes.len() >= MAX_CLASSES && !classes.contains_key(obf) {
                    return Err(ProguardError::TooLarge);
                }
                classes.entry(obf.to_owned()).or_default().original = original.to_owned();
                current = Some(obf.to_owned());
            } else if let Some(class) = &current
                && let Some((obf_name, method)) = method_line(line.trim())
            {
                methods += 1;
                if methods > MAX_METHODS {
                    return Err(ProguardError::TooLarge);
                }
                if let Some(class) = classes.get_mut(class) {
                    class.methods.entry(obf_name).or_default().push(method);
                }
            }
        }
        if classes.is_empty() {
            return Err(ProguardError::Empty);
        }
        Ok(Self { classes, methods })
    }

    /// The original frames for an obfuscated class, method and line (0 when
    /// the stack has no line). `None` when the class or the method is not in
    /// the mapping.
    pub fn retrace(&self, class: &str, method: &str, line: u32) -> Option<Vec<Retraced>> {
        let class = self.classes.get(class)?;
        let candidates = class.methods.get(method)?;
        let mut found: Vec<Retraced> = Vec::new();
        for m in candidates {
            if let Some((start, end)) = m.obf_range
                && line >= start
                && line <= end
            {
                found.push(self.frame(class, m, line, start));
            }
        }
        if found.is_empty() {
            // Lines the mapping has no range for, or a stack without lines:
            // methods that have no range, or the only method of that name.
            let mut unranged = candidates.iter().filter(|m| m.obf_range.is_none());
            if let Some(m) = unranged.next() {
                found.push(self.frame(class, m, 0, 0));
                found.extend(unranged.map(|m| self.frame(class, m, 0, 0)));
            } else if line == 0 && candidates.len() == 1 {
                found.push(self.frame(class, &candidates[0], 0, 0));
            }
        }
        (!found.is_empty()).then_some(found)
    }

    fn frame(&self, class: &Class, m: &Method, line: u32, obf_start: u32) -> Retraced {
        let original_line = match m.original_range {
            Some((start, end)) if line > 0 => {
                start.saturating_add(line - obf_start).min(end.max(start))
            }
            Some((start, _)) => start,
            None => 0,
        };
        Retraced {
            class: m.class.clone().unwrap_or_else(|| class.original.clone()),
            method: m.name.clone(),
            line: original_line,
        }
    }

    pub fn approx_bytes(&self) -> usize {
        self.classes
            .iter()
            .map(|(k, c)| k.len() + c.original.len() + 64)
            .sum::<usize>()
            + self.methods * 96
    }
}

/// Packages of the platform and common libraries, which are not the app's own
/// code.
const FOREIGN: &[&str] = &[
    "android.",
    "androidx.",
    "java.",
    "javax.",
    "kotlin.",
    "kotlinx.",
    "dalvik.",
    "libcore.",
    "com.android.",
    "sun.",
    "org.jetbrains.",
];

/// The frames a Java frame (class in `module`, method in `function`, line in
/// `line`) stands for. The frame itself when the mapping does not know it.
pub fn symbolicate(map: &ProguardMap, frame: &Frame) -> Vec<Frame> {
    match map.retrace(&frame.module, &frame.function, frame.line) {
        None => vec![frame.clone()],
        Some(found) => found
            .into_iter()
            .map(|r| Frame {
                in_app: frame.in_app && !FOREIGN.iter().any(|p| r.class.starts_with(p)),
                module: r.class,
                function: r.method,
                line: r.line,
                column: 0,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPPING: &str = "\
# compiler: R8
# compiler_version: 8.5.35
# pg_map_id: abc
com.example.Cart -> a.a:
    int count -> a
    java.util.List items -> b
    1:1:void <init>():10:10 -> <init>
    12:15:void checkout(int):40:43 -> a
    16:16:void com.example.Util.round(double):7:7 -> a
    16:16:void checkout(int):44:44 -> a
    20:25:void reset():100 -> b
    void noLines() -> c
com.example.Util -> a.b:
    1:3:double round(double):5:7 -> a
com.example.Plain -> com.example.Plain:
    void run() -> run
";

    fn map() -> ProguardMap {
        ProguardMap::parse(MAPPING.as_bytes(), 1 << 20).expect("parse")
    }

    fn at(class: &str, method: &str, line: u32) -> Vec<(String, String, u32)> {
        map()
            .retrace(class, method, line)
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.class, r.method, r.line))
            .collect()
    }

    fn t(class: &str, method: &str, line: u32) -> (String, String, u32) {
        (class.to_owned(), method.to_owned(), line)
    }

    #[test]
    fn a_line_in_a_range_maps_to_the_original_line() {
        assert_eq!(
            at("a.a", "a", 12),
            vec![t("com.example.Cart", "checkout", 40)]
        );
        assert_eq!(
            at("a.a", "a", 15),
            vec![t("com.example.Cart", "checkout", 43)]
        );
        assert_eq!(at("a.b", "a", 2), vec![t("com.example.Util", "round", 6)]);
        assert_eq!(
            at("a.a", "<init>", 1),
            vec![t("com.example.Cart", "<init>", 10)]
        );
    }

    #[test]
    fn an_inlined_line_returns_the_callee_first_and_then_the_caller() {
        assert_eq!(
            at("a.a", "a", 16),
            vec![
                t("com.example.Util", "round", 7),
                t("com.example.Cart", "checkout", 44)
            ]
        );
    }

    #[test]
    fn a_range_that_gives_one_original_line_maps_every_line_to_it() {
        assert_eq!(
            at("a.a", "b", 22),
            vec![t("com.example.Cart", "reset", 100)]
        );
    }

    #[test]
    fn a_method_without_lines_matches_any_line_or_none() {
        assert_eq!(at("a.a", "c", 0), vec![t("com.example.Cart", "noLines", 0)]);
        assert_eq!(
            at("a.a", "c", 77),
            vec![t("com.example.Cart", "noLines", 0)]
        );
        assert_eq!(
            at("com.example.Plain", "run", 3),
            vec![t("com.example.Plain", "run", 0)]
        );
    }

    #[test]
    fn what_the_mapping_does_not_know_is_left_alone() {
        let map = map();
        assert!(map.retrace("x.y", "a", 1).is_none(), "unknown class");
        assert!(map.retrace("a.a", "zzz", 1).is_none(), "unknown method");
        assert!(
            map.retrace("a.a", "a", 99).is_none(),
            "a line outside every range"
        );
        let frame = Frame {
            module: "x.y".to_owned(),
            function: "a".to_owned(),
            line: 1,
            column: 0,
            in_app: true,
        };
        assert_eq!(symbolicate(&map, &frame), vec![frame.clone()]);
    }

    #[test]
    fn a_frame_becomes_original_frames_and_platform_classes_are_not_the_apps() {
        let map = ProguardMap::parse(
            b"android.app.Activity -> a.c:\n    1:2:void onCreate():5:6 -> a\ncom.example.Main -> a.d:\n    1:2:void run():5:6 -> a\n",
            1 << 20,
        )
        .expect("parse");
        let frame = |module: &str| Frame {
            module: module.to_owned(),
            function: "a".to_owned(),
            line: 2,
            column: 0,
            in_app: true,
        };
        let platform = symbolicate(&map, &frame("a.c"));
        assert_eq!(
            (
                platform[0].module.as_str(),
                platform[0].function.as_str(),
                platform[0].line,
                platform[0].in_app
            ),
            ("android.app.Activity", "onCreate", 6, false)
        );
        let own = symbolicate(&map, &frame("a.d"));
        assert_eq!(
            (
                own[0].module.as_str(),
                own[0].function.as_str(),
                own[0].line,
                own[0].in_app
            ),
            ("com.example.Main", "run", 6, true)
        );
    }

    #[test]
    fn junk_lines_are_skipped_and_an_empty_mapping_is_refused() {
        let text = "com.example.A -> a.a:\n    garbage\n    1:2:void ok():3:4 -> a\n    x:y:void bad():3 -> b\n    1:2:void worse(:3 -> c\n    4:3:void reversed():1:1 -> d\n";
        let map = ProguardMap::parse(text.as_bytes(), 1 << 20).expect("parse");
        assert_eq!(map.retrace("a.a", "a", 1).map(|f| f.len()), Some(1));
        for m in ["b", "c", "d"] {
            assert!(map.retrace("a.a", m, 1).is_none(), "{m}");
        }
        assert_eq!(
            ProguardMap::parse(b"", 1 << 20).unwrap_err(),
            ProguardError::Empty
        );
        assert_eq!(
            ProguardMap::parse(b"just text\n# c\n", 1 << 20).unwrap_err(),
            ProguardError::Empty
        );
        assert_eq!(
            ProguardMap::parse(b"\xff\xfe", 1 << 20).unwrap_err(),
            ProguardError::NotText
        );
        assert_eq!(
            ProguardMap::parse(MAPPING.as_bytes(), 10).unwrap_err(),
            ProguardError::TooLarge
        );
    }

    #[test]
    fn a_very_long_line_is_skipped_not_trusted() {
        let long = format!(
            "com.example.A -> a.a:\n    1:2:void {}():3:4 -> a\n    1:2:void ok():3:4 -> b\n",
            "m".repeat(MAX_LINE)
        );
        let map = ProguardMap::parse(long.as_bytes(), 1 << 30).expect("parse");
        assert!(map.retrace("a.a", "a", 1).is_none());
        assert!(map.retrace("a.a", "b", 1).is_some());
    }
}
