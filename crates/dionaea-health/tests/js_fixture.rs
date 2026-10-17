//! A real minified bundle and its source map, made by esbuild 0.25 from the
//! three files in `fixtures/js/src` (minified, with source maps). The expected
//! positions are not computed by this crate: they are what Node itself printed
//! for the same bundle with `--enable-source-maps`, for the stack of a
//! `TypeError` thrown from `parseQuantity`:
//!
//! ```text
//! at parseQuantity (src/util.js:4:11)
//! at <anonymous>   (src/util.js:10:57)
//! at total         (src/util.js:10:16)
//! at o.checkout    (src/cart.js:9:20)
//! at submitOrder   (src/index.js:5:15)
//! at Object.<anonymous> (src/index.js:10:3)
//! ```
//!
//! and the minified positions Node printed without source maps
//! (`app.min.js:1:56`, `1:158`, `1:134`, `1:240`, `1:311`, `1:368`).

use std::fs;
use std::path::Path;

use dionaea_health::sourcemap::SourceMap;

fn map() -> SourceMap {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/js/app.min.js.map");
    let bytes = fs::read(path).expect("the fixture exists");
    SourceMap::parse(&bytes, 1 << 20).expect("a valid source map")
}

#[test]
fn the_frames_of_a_real_stack_map_to_the_files_and_lines_node_reports() {
    let map = map();
    let expected = [
        (56, "util.js", 4, 11),
        (158, "util.js", 10, 57),
        (134, "util.js", 10, 16),
        (240, "cart.js", 9, 20),
        (311, "index.js", 5, 15),
        (368, "index.js", 10, 3),
    ];
    for (column, file, line, original_column) in expected {
        let found = map
            .lookup(1, column)
            .unwrap_or_else(|| panic!("nothing maps at 1:{column}"));
        assert!(
            found.source.ends_with(file),
            "1:{column} is in {} and not {file}",
            found.source
        );
        assert_eq!(
            (found.line, found.column),
            (line, original_column),
            "1:{column}"
        );
    }
}

#[test]
fn the_function_around_each_frame_is_read_from_the_original_source() {
    let map = map();
    // Node's names, except where its answer is the arrow function
    // (`<anonymous>`) and ours is the function around it, and the top level.
    let expected: [(u32, Option<&str>); 6] = [
        (56, Some("parseQuantity")),
        (158, Some("total")),
        (134, Some("total")),
        (240, Some("checkout")),
        (311, Some("submitOrder")),
        (368, None),
    ];
    for (column, name) in expected {
        let found = map.lookup(1, column).expect("a mapping");
        assert_eq!(map.function_name(&found).as_deref(), name, "1:{column}");
    }
}

#[test]
fn the_bundle_is_one_line_and_the_map_is_small() {
    let map = map();
    assert!(map.approx_bytes() < 16 * 1024, "{}", map.approx_bytes());
    assert!(map.lookup(2, 1).is_none());
}

#[test]
fn a_frame_with_no_named_function_is_anonymous_and_not_the_minified_name() {
    use dionaea_health::frames::Frame;
    use dionaea_health::sourcemap::symbolicate;

    let map = map();
    let frame = |column, function: &str| Frame {
        module: "app.min.js".to_owned(),
        function: function.to_owned(),
        line: 1,
        column,
        in_app: true,
    };
    // The top level of index.js: the stack called it something Node-specific.
    let top = symbolicate(&map, &frame(368, "Object.<anonymous>")).expect("mapped");
    assert_eq!(
        (top.module.as_str(), top.function.as_str()),
        ("src/index.js", "<anonymous>")
    );
    // And the same position under another minified name gives the same frame.
    let other = symbolicate(&map, &frame(368, "q")).expect("mapped");
    assert_eq!(top, other);
    let named = symbolicate(&map, &frame(56, "u")).expect("mapped");
    assert_eq!(
        (
            named.module.as_str(),
            named.function.as_str(),
            named.line,
            named.column
        ),
        ("src/util.js", "parseQuantity", 4, 11)
    );
}
