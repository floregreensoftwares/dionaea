//! Grouping errors into issues: a fingerprint of an error that stays the same
//! when the code around it changes a little, and differs when the error does.
//!
//! The fingerprint is a hash of the exception type and the **in-app frames**
//! (module and function, never line or column, so adding a line above a
//! function does not split its issue). Up to [`MAX_FRAMES_USED`] frames count,
//! so a recursion that is one level deeper on another run groups with the
//! rest. A stack with no in-app frame uses its first [`FALLBACK_FRAMES`]
//! frames. Module names lose their scheme, host, query, and the content hashes
//! bundlers put in file names (`main.3f9a1b2c.js` is `main.js`), so the same
//! error in two releases is one issue. Function names that say nothing
//! (`<anonymous>`) all count as one.
//!
//! The rules are versioned (`v1`): a deliberate change makes a new version,
//! because it would regroup every issue.

use sha2::{Digest, Sha256};

use crate::frames::Frame;

/// Most in-app frames that count.
pub const MAX_FRAMES_USED: usize = 10;
/// Frames that count when none is in-app.
pub const FALLBACK_FRAMES: usize = 5;
const VERSION: &str = "v1";

/// The 32-character fingerprint of an error.
pub fn fingerprint(error_type: &str, frames: &[Frame]) -> String {
    let mut chosen: Vec<&Frame> = frames
        .iter()
        .filter(|f| f.in_app)
        .take(MAX_FRAMES_USED)
        .collect();
    if chosen.is_empty() {
        chosen = frames.iter().take(FALLBACK_FRAMES).collect();
    }
    let mut hasher = Sha256::new();
    hasher.update(VERSION.as_bytes());
    hasher.update(b"\n");
    hasher.update(error_type.as_bytes());
    for frame in chosen {
        hasher.update(b"\n--\n");
        hasher.update(normalize_module(&frame.module).as_bytes());
        hasher.update(b"\n");
        hasher.update(normalize_function(&frame.function).as_bytes());
    }
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .fold(String::with_capacity(32), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// A module without scheme, host, query or fragment, and without the content
/// hash in its file name.
pub fn normalize_module(module: &str) -> String {
    let without_query = module.split(['?', '#']).next().unwrap_or("");
    let path = match without_query.find("://") {
        Some(at) => {
            let rest = &without_query[at + 3..];
            rest.find('/').map_or("", |slash| &rest[slash..])
        }
        None => without_query,
    };
    let (dir, file) = match path.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, path),
    };
    let file = strip_content_hash(file);
    match dir {
        Some(dir) => format!("{dir}/{file}"),
        None => file,
    }
}

fn is_hash(part: &str) -> bool {
    part.len() >= 8
        && part.bytes().all(|b| b.is_ascii_hexdigit())
        && part.bytes().any(|b| b.is_ascii_digit())
}

/// `main.3f9a1b2c.js` and `main-3f9a1b2c.js` become `main.js`.
fn strip_content_hash(file: &str) -> String {
    let parts: Vec<&str> = file.split('.').collect();
    let mut kept: Vec<String> = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        // The first part is the name: it may end in `-hash` or `_hash`.
        if index == 0 {
            let name = match part.rsplit_once(['-', '_']) {
                Some((name, tail)) if !name.is_empty() && is_hash(tail) => name,
                _ => part,
            };
            kept.push(name.to_owned());
        } else if index + 1 < parts.len() && is_hash(part) {
            continue;
        } else {
            kept.push((*part).to_owned());
        }
    }
    kept.join(".")
}

/// A function name without what varies between runs or releases.
pub fn normalize_function(function: &str) -> String {
    let name = function.trim();
    let name = name.strip_prefix("async ").unwrap_or(name);
    let name = name.split(" [as ").next().unwrap_or(name);
    let name = name.strip_prefix("Object.").unwrap_or(name);
    if name.is_empty() || matches!(name, "<anonymous>" | "anonymous" | "?" | "<unknown>") {
        "?".to_owned()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(module: &str, function: &str, line: u32, in_app: bool) -> Frame {
        Frame {
            module: module.to_owned(),
            function: function.to_owned(),
            line,
            column: 7,
            in_app,
        }
    }

    fn stack() -> Vec<Frame> {
        vec![
            frame("src/util.js", "parseQuantity", 4, true),
            frame("src/cart.js", "checkout", 9, true),
            frame("node_modules/react/index.js", "render", 100, false),
        ]
    }

    #[test]
    fn a_line_number_or_a_library_frame_does_not_change_the_fingerprint() {
        let base = fingerprint("TypeError", &stack());
        let mut moved = stack();
        moved[0].line = 40;
        moved[1].column = 99;
        moved[2] = frame("node_modules/react/other.js", "commit", 7, false);
        assert_eq!(fingerprint("TypeError", &moved), base);
        assert_eq!(base.len(), 32);
    }

    #[test]
    fn another_type_function_or_module_is_another_issue() {
        let base = fingerprint("TypeError", &stack());
        assert_ne!(fingerprint("RangeError", &stack()), base);
        let mut renamed = stack();
        renamed[0].function = "parseAmount".to_owned();
        assert_ne!(fingerprint("TypeError", &renamed), base);
        let mut elsewhere = stack();
        elsewhere[1].module = "src/order.js".to_owned();
        assert_ne!(fingerprint("TypeError", &elsewhere), base);
        let shorter: Vec<Frame> = stack().into_iter().take(1).collect();
        assert_ne!(fingerprint("TypeError", &shorter), base);
    }

    #[test]
    fn only_the_first_in_app_frames_count() {
        let deep: Vec<Frame> = (0..30)
            .map(|i| frame("src/a.js", "recurse", i, true))
            .collect();
        let deeper: Vec<Frame> = (0..45)
            .map(|i| frame("src/a.js", "recurse", i, true))
            .collect();
        assert_eq!(fingerprint("E", &deep), fingerprint("E", &deeper));
    }

    #[test]
    fn a_stack_with_no_app_frame_uses_its_first_frames() {
        let library: Vec<Frame> = (0..8)
            .map(|i| frame("lib/a.js", &format!("f{i}"), 1, false))
            .collect();
        let mut tail_changed = library.clone();
        tail_changed[7].function = "other".to_owned();
        assert_eq!(
            fingerprint("E", &library),
            fingerprint("E", &tail_changed),
            "past the fifth frame"
        );
        tail_changed[2].function = "other".to_owned();
        assert_ne!(fingerprint("E", &library), fingerprint("E", &tail_changed));
        assert_eq!(fingerprint("E", &[]), fingerprint("E", &[]));
    }

    #[test]
    fn modules_lose_their_host_query_and_content_hash() {
        for (module, expected) in [
            (
                "https://cdn.example.com/assets/main.3f9a1b2c.js?v=1#x",
                "/assets/main.js",
            ),
            (
                "https://other.example.com/assets/main.3f9a1b2c.js",
                "/assets/main.js",
            ),
            ("assets/main-3f9a1b2c.js", "assets/main.js"),
            ("assets/chunk_0a1b2c3d4e.js", "assets/chunk.js"),
            ("assets/app.js", "assets/app.js"),
            ("assets/v1.2.3.min.js", "assets/v1.2.3.min.js"),
            ("assets/vendor.dea0beef.min.js", "assets/vendor.min.js"),
            ("assets/face-abcdefgh.js", "assets/face-abcdefgh.js"),
            ("com.example.Cart", "com.example.Cart"),
            ("", ""),
        ] {
            assert_eq!(normalize_module(module), expected, "{module}");
        }
    }

    #[test]
    fn meaningless_function_names_are_one_name() {
        for name in ["", " ", "<anonymous>", "anonymous", "?", "<unknown>"] {
            assert_eq!(normalize_function(name), "?", "{name:?}");
        }
        assert_eq!(normalize_function("async checkout"), "checkout");
        assert_eq!(normalize_function("Object.run"), "run");
        assert_eq!(
            normalize_function("Cart.checkout [as buy]"),
            "Cart.checkout"
        );
    }

    #[test]
    fn the_same_error_in_two_releases_is_one_issue() {
        let release_a = vec![
            frame(
                "https://cdn.example.com/static/app.3f9a1b2c.js",
                "checkout",
                120,
                true,
            ),
            frame(
                "https://cdn.example.com/static/app.3f9a1b2c.js",
                "submit",
                90,
                true,
            ),
        ];
        let release_b = vec![
            frame(
                "https://cdn2.example.com/static/app.77ee00aa.js",
                "checkout",
                133,
                true,
            ),
            frame(
                "https://cdn2.example.com/static/app.77ee00aa.js",
                "submit",
                101,
                true,
            ),
        ];
        assert_eq!(
            fingerprint("TypeError", &release_a),
            fingerprint("TypeError", &release_b)
        );
    }

    /// Pins version 1: if this changes, every issue is regrouped, so it must be
    /// a new version on purpose.
    #[test]
    fn version_one_is_pinned() {
        assert_eq!(
            fingerprint("TypeError", &stack()),
            "b6cadf250b5ae6993a6d6ebabe8e3177"
        );
    }
}
