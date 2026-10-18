//! Symbolication of stored error rows against real files: the esbuild bundle
//! and source map of `dionaea-health`'s fixtures, and a mapping written by
//! hand, through the real metadata index and artifact directory.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dionaea_artifacts::{ArtifactId, ArtifactStore, LocalArtifacts};
use dionaea_health::frames::{self, Frame};
use dionaea_meta::{MetaStore, SqliteMeta, SymbolFile, SymbolIndex, SymbolKind};
use dionaea_store::{EventRow, Store, event_key};
use serde_json::Value;

use crate::symbolicate::{SymbolConfig, Symbolicator};

const SITE: u32 = 1;
const RELEASE: &str = "1.4.2+57";
const EVENT_ID: &str = "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e";

const MAPPING: &str = "\
com.example.Cart -> a.a:
    12:15:void checkout(int):40:43 -> a
    16:16:void com.example.Util.round(double):7:7 -> a
    16:16:void checkout(int):44:44 -> a
";

fn fixture_map() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../dionaea-health/tests/fixtures/js/app.min.js.map");
    fs::read(path).expect("the fixture exists")
}

struct World {
    meta: Arc<SqliteMeta>,
    artifacts: Arc<LocalArtifacts>,
    dir: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn world(tag: &str) -> World {
    let dir =
        std::env::temp_dir().join(format!("dionaea-symbolicate-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let meta = SqliteMeta::open_in_memory().expect("meta");
    let organization = meta.create_organization("o").expect("organization");
    assert_eq!(
        meta.create_site(organization.id, "s").expect("site").id,
        SITE
    );
    World {
        meta: Arc::new(meta),
        artifacts: Arc::new(LocalArtifacts::open(&dir).expect("artifacts")),
        dir,
    }
}

impl World {
    fn upload(&self, release: &str, kind: SymbolKind, name: &str, bytes: &[u8]) {
        let stored = self
            .artifacts
            .put(
                ArtifactId {
                    site_id: SITE,
                    release,
                    kind: kind.as_str(),
                    name,
                },
                bytes,
            )
            .expect("stored");
        self.meta
            .record_symbol_file(&SymbolFile {
                site_id: SITE,
                release: release.to_owned(),
                kind,
                name: name.to_owned(),
                size: stored.size,
                sha256: stored.sha256,
                uploaded_at: 1,
            })
            .expect("indexed");
    }

    fn symbolicator(&self, config: SymbolConfig) -> Symbolicator {
        Symbolicator::new(
            Arc::clone(&self.meta) as Arc<dyn SymbolIndex>,
            Arc::clone(&self.artifacts) as Arc<dyn ArtifactStore>,
            config,
        )
    }
}

fn frame(module: &str, function: &str, line: u32, column: u32) -> Frame {
    Frame {
        module: module.to_owned(),
        function: function.to_owned(),
        line,
        column,
        in_app: true,
    }
}

/// The row of an `app_error` as the batcher reads it from the spool.
fn error_row(id: &str, platform: &str, release: &str, frames: &[Frame]) -> String {
    let mut row = EventRow {
        site_id: SITE,
        event_id: id.to_owned(),
        ts_ms: 1_760_000_000_000,
        received_ms: 1_760_000_001_000,
        name: "app_error".to_owned(),
        schema_version: 1,
        anon_id: "a1".to_owned(),
        session_id: "s1".to_owned(),
        screen: "/cart".to_owned(),
        props_str: vec![
            ("kind".to_owned(), "unhandled".to_owned()),
            ("type".to_owned(), "TypeError".to_owned()),
            ("message".to_owned(), "bad quantity".to_owned()),
        ],
        props_num: Vec::new(),
        country: "BR".to_owned(),
        platform: platform.to_owned(),
        os: String::new(),
        browser: String::new(),
        device_class: String::new(),
        app_version: release.to_owned(),
        sdk: "test/1".to_owned(),
    };
    if !frames.is_empty() {
        row.props_str
            .push(("frames".to_owned(), frames::encode(frames)));
    }
    Store::encode_events(&[row]).expect("encoded")
}

fn props(row: &str) -> serde_json::Map<String, Value> {
    let value: Value = serde_json::from_str(row.trim_end()).expect("a json row");
    value["props_str"].as_object().expect("props").clone()
}

fn text<'a>(map: &'a serde_json::Map<String, Value>, name: &str) -> &'a str {
    map.get(name).and_then(Value::as_str).unwrap_or("")
}

/// The stack Node printed for the fixture bundle, as the SDK would send it.
fn minified_stack() -> Vec<Frame> {
    [56, 158, 134, 240, 311, 368]
        .into_iter()
        .map(|column| frame("https://app.example.com/app.min.js", "x", 1, column))
        .collect()
}

#[test]
fn a_real_minified_stack_comes_back_as_the_original_code() {
    let world = world("real");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let row = error_row(EVENT_ID, "web", RELEASE, &minified_stack());

    let out = symbolicator.process(row.clone());
    let map = props(&out);
    assert_eq!(text(&map, "symbols"), "ok");
    let frames = frames::parse(text(&map, "frames"));
    let seen: Vec<(String, &str, u32, u32)> = frames
        .iter()
        .map(|f| {
            (
                f.module.rsplit('/').next().unwrap_or("").to_owned(),
                f.function.as_str(),
                f.line,
                f.column,
            )
        })
        .collect();
    let expected = [
        ("util.js", "parseQuantity", 4, 11),
        ("util.js", "total", 10, 57),
        ("util.js", "total", 10, 16),
        ("cart.js", "checkout", 9, 20),
        ("index.js", "submitOrder", 5, 15),
        ("index.js", "<anonymous>", 10, 3),
    ];
    assert_eq!(seen.len(), expected.len(), "{seen:?}");
    for (got, want) in seen.iter().zip(expected) {
        assert_eq!((got.0.as_str(), got.1, got.2, got.3), want);
    }
    assert_eq!(text(&map, "fingerprint").len(), 32);
    // The rest of the row, and the id the batcher deduplicates on, are as they were.
    assert_eq!(event_key(&out), event_key(&row));
    assert_eq!(text(&map, "message"), "bad quantity");
    assert_eq!(text(&map, "type"), "TypeError");
    assert!(out.ends_with('\n') && out.matches('\n').count() == 1);
}

#[test]
fn the_fingerprint_is_the_same_for_a_minified_build_of_another_release() {
    let world = world("release");
    world.upload(
        "1.4.2+57",
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    world.upload(
        "1.4.3+60",
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let print = |release: &str| {
        let row = error_row(EVENT_ID, "web", release, &minified_stack());
        text(&props(&symbolicator.process(row)), "fingerprint").to_owned()
    };
    assert_eq!(print("1.4.2+57"), print("1.4.3+60"));
}

#[test]
fn an_event_with_no_file_for_its_release_is_stored_raw_and_flagged() {
    let world = world("missing");
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let stack = minified_stack();
    let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &stack));
    let map = props(&out);
    assert_eq!(text(&map, "symbols"), "missing");
    assert_eq!(text(&map, "frames"), frames::encode(&stack));
    // Still grouped, on the raw frames.
    assert_eq!(text(&map, "fingerprint").len(), 32);
    // No release at all is the same.
    let out = symbolicator.process(error_row(EVENT_ID, "web", "", &stack));
    assert_eq!(text(&props(&out), "symbols"), "missing");
}

#[test]
fn a_file_that_cannot_be_read_is_failed_not_missing() {
    let world = world("failed");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        b"not json",
    );
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &minified_stack()));
    assert_eq!(text(&props(&out), "symbols"), "failed");
}

#[test]
fn some_frames_without_a_file_make_the_event_partial() {
    let world = world("partial");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let mut stack = minified_stack();
    stack.push(frame("https://app.example.com/vendor.js", "y", 1, 5));
    let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &stack));
    let map = props(&out);
    assert_eq!(text(&map, "symbols"), "partial");
    let frames = frames::parse(text(&map, "frames"));
    assert_eq!(frames.len(), 7);
    assert_eq!(frames[6].module, "https://app.example.com/vendor.js");
}

#[test]
fn an_android_stack_is_retraced_with_inlined_frames() {
    let world = world("android");
    world.upload(
        RELEASE,
        SymbolKind::Proguard,
        "mapping.txt",
        MAPPING.as_bytes(),
    );
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let stack = [frame("a.a", "a", 13, 0), frame("a.a", "a", 16, 0)];
    let out = symbolicator.process(error_row(EVENT_ID, "android", RELEASE, &stack));
    let map = props(&out);
    assert_eq!(text(&map, "symbols"), "ok");
    let frames = frames::parse(text(&map, "frames"));
    let shown: Vec<(&str, &str, u32)> = frames
        .iter()
        .map(|f| (f.module.as_str(), f.function.as_str(), f.line))
        .collect();
    assert_eq!(
        shown,
        [
            ("com.example.Cart", "checkout", 41),
            ("com.example.Util", "round", 7),
            ("com.example.Cart", "checkout", 44),
        ]
    );
    // The same stack on another platform has no mapping to use.
    let out = symbolicator.process(error_row(EVENT_ID, "ios", RELEASE, &stack));
    assert_eq!(text(&props(&out), "symbols"), "none");
}

#[test]
fn rows_that_are_not_errors_pass_through_byte_for_byte() {
    let world = world("others");
    let symbolicator = world.symbolicator(SymbolConfig::default());
    let mut other = EventRow {
        name: "screen".to_owned(),
        ..serde_row()
    };
    other.props_str = vec![("name".to_owned(), "app_error".to_owned())];
    let other = Store::encode_events(&[other]).unwrap();
    assert_eq!(symbolicator.process(other.clone()), other);

    let error = error_row(EVENT_ID, "web", RELEASE, &minified_stack());
    let mixed = format!("{other}{error}{other}");
    let out = symbolicator.process(mixed);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(format!("{}\n", lines[0]), other);
    assert_eq!(format!("{}\n", lines[2]), other);
    assert_eq!(text(&props(lines[1]), "symbols"), "missing");
}

fn serde_row() -> EventRow {
    let row = error_row(EVENT_ID, "web", RELEASE, &[]);
    let value: Value = serde_json::from_str(row.trim_end()).unwrap();
    EventRow {
        site_id: SITE,
        event_id: EVENT_ID.to_owned(),
        ts_ms: 1_760_000_000_000,
        received_ms: 1_760_000_001_000,
        name: String::new(),
        schema_version: 1,
        anon_id: "a1".to_owned(),
        session_id: String::new(),
        screen: String::new(),
        props_str: Vec::new(),
        props_num: Vec::new(),
        country: String::new(),
        platform: value["platform"].as_str().unwrap().to_owned(),
        os: String::new(),
        browser: String::new(),
        device_class: String::new(),
        app_version: String::new(),
        sdk: "test/1".to_owned(),
    }
}

#[test]
fn a_batch_past_its_deadline_is_stored_raw_and_flagged_skipped() {
    let world = world("late");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig {
        deadline: Duration::ZERO,
        ..SymbolConfig::default()
    });
    let stack = minified_stack();
    let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &stack));
    let map = props(&out);
    assert_eq!(text(&map, "symbols"), "skipped");
    assert_eq!(text(&map, "frames"), frames::encode(&stack));
    assert_eq!(text(&map, "fingerprint").len(), 32);
}

#[test]
fn a_replaced_file_is_noticed_after_the_recheck_and_not_before() {
    let world = world("replace");
    let config = |recheck| SymbolConfig {
        recheck,
        ..SymbolConfig::default()
    };
    let slow = world.symbolicator(config(Duration::from_secs(3600)));
    let fast = world.symbolicator(config(Duration::ZERO));
    let row = || error_row(EVENT_ID, "web", RELEASE, &minified_stack());
    let symbols = |s: &Symbolicator| text(&props(&s.process(row())), "symbols").to_owned();

    assert_eq!(symbols(&slow), "missing");
    assert_eq!(symbols(&fast), "missing");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    assert_eq!(symbols(&slow), "missing", "still trusted");
    assert_eq!(symbols(&fast), "ok");
    // The same file is not read again.
    let loads = fast.stats().loads();
    assert_eq!(symbols(&fast), "ok");
    assert_eq!(fast.stats().loads(), loads);
    // A replacement is read.
    let mut changed = fixture_map();
    changed.push(b' ');
    world.upload(RELEASE, SymbolKind::SourceMap, "app.min.js.map", &changed);
    assert_eq!(symbols(&fast), "ok");
    assert_eq!(fast.stats().loads(), loads + 1);
}

#[test]
fn the_cache_stays_inside_its_bound_whatever_releases_are_claimed() {
    let world = world("bound");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig {
        cache_bytes: 64 * 1024,
        ..SymbolConfig::default()
    });
    for n in 0..2000 {
        let release = format!("9.{n}");
        symbolicator.process(error_row(EVENT_ID, "web", &release, &minified_stack()));
    }
    let stats = symbolicator.stats();
    assert!(stats.cache_bytes() <= 64 * 1024, "{}", stats.cache_bytes());
    assert!(stats.cache_entries() < 2000);
    // The real release still works after the flood.
    let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &minified_stack()));
    assert_eq!(text(&props(&out), "symbols"), "ok");
}

#[test]
fn a_file_too_large_for_the_cache_is_failed_once_and_not_read_every_time() {
    let world = world("large");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig {
        cache_bytes: 1024,
        ..SymbolConfig::default()
    });
    for _ in 0..5 {
        let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &minified_stack()));
        assert_eq!(text(&props(&out), "symbols"), "failed");
    }
    assert_eq!(symbolicator.stats().loads(), 1);
}

#[test]
fn many_errors_across_workers_all_come_back_in_order() {
    let world = world("workers");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig {
        workers: 4,
        ..SymbolConfig::default()
    });
    let mut batch = String::new();
    let mut ids = Vec::new();
    for n in 0..200u32 {
        let id = format!("0190f7e2-7c3e-7a52-9a0e-5f3c2a1b{n:04x}");
        batch.push_str(&error_row(&id, "web", RELEASE, &minified_stack()));
        ids.push(id);
    }
    let out = symbolicator.process(batch);
    let seen: Vec<String> = out
        .lines()
        .map(|line| event_key(line).expect("a key").1.to_owned())
        .collect();
    assert_eq!(seen, ids);
    assert!(out.lines().all(|l| text(&props(l), "symbols") == "ok"));
    assert_eq!(
        symbolicator.stats().loads(),
        1,
        "one read however many workers"
    );
}

#[test]
fn a_flood_of_errors_cannot_hold_a_batch_past_its_deadline_by_much() {
    let world = world("flood");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig {
        deadline: Duration::from_millis(100),
        ..SymbolConfig::default()
    });
    // 20,000 errors, each claiming a release of its own: every one needs an
    // index lookup, and none has a file.
    let mut batch = String::new();
    for n in 0..20_000u32 {
        let id = format!("0190f7e2-7c3e-7a52-9a0e-{n:012x}");
        batch.push_str(&error_row(&id, "web", &format!("2.{n}"), &minified_stack()));
    }
    let started = std::time::Instant::now();
    let out = symbolicator.process(batch);
    let took = started.elapsed();
    assert_eq!(out.lines().count(), 20_000);
    let outcomes = symbolicator.stats().outcomes();
    let total: u64 = outcomes.iter().map(|(_, n)| n).sum();
    assert_eq!(total, 20_000, "{outcomes:?}");
    // Past the deadline an event costs a parse and a rewrite, not a lookup.
    assert!(took < Duration::from_secs(10), "{took:?}");
    eprintln!("flood: {took:?}, {outcomes:?}");
}

#[test]
fn invented_releases_cannot_push_a_real_file_out_of_the_cache() {
    let world = world("pollution");
    world.upload(
        RELEASE,
        SymbolKind::SourceMap,
        "app.min.js.map",
        &fixture_map(),
    );
    let symbolicator = world.symbolicator(SymbolConfig {
        // Small enough that the flood alone would fill it.
        cache_bytes: 512 * 1024,
        ..SymbolConfig::default()
    });
    let real = || {
        let out = symbolicator.process(error_row(EVENT_ID, "web", RELEASE, &minified_stack()));
        text(&props(&out), "symbols").to_owned()
    };
    assert_eq!(real(), "ok");
    for n in 0..6000 {
        let release = format!("8.{n}");
        symbolicator.process(error_row(EVENT_ID, "web", &release, &minified_stack()[..1]));
    }
    assert_eq!(real(), "ok");
    assert_eq!(symbolicator.stats().loads(), 1, "never read twice");
    assert!(symbolicator.stats().cache_entries() <= 4097);
}
