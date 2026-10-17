//! What the server pays per health event for the rules in this crate.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dionaea_health::fingerprint::fingerprint;
use dionaea_health::frames::{Frame, parse};
use dionaea_health::proguard::ProguardMap;
use dionaea_health::sourcemap::SourceMap;
use dionaea_health::{normalize_frames, scrub_message, template_route};

const JS_MAP: &[u8] = include_bytes!("../tests/fixtures/js/app.min.js.map");
const MAPPING: &str = "com.example.Cart -> a.a:\n    12:15:void checkout(int):40:43 -> a\n    16:16:void com.example.Util.round(double):7:7 -> a\n    16:16:void checkout(int):44:44 -> a\ncom.example.Util -> a.b:\n    1:3:double round(double):5:7 -> a\n";

const CLEAN: &str = "TypeError: Cannot read properties of undefined (reading 'items') at checkout";
const DIRTY: &str = "failed for ana@example.com at https://api.example.com/v1/orders/123456?token=abcdef order 0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e card 4111 1111 1111 1111";
const FRAMES: &str = "app/cart.js|checkout|42|7|1;app/store.js|dispatch|10|3|1;lib/react.js|render|1000|3|0;lib/react.js|commit|2000|3|0;https://cdn.example.com/app.js?v=3|anonymous|1|90210|0";

fn rules(c: &mut Criterion) {
    c.bench_function("scrub_clean_message", |b| {
        b.iter(|| scrub_message(black_box(CLEAN)));
    });
    c.bench_function("scrub_message_with_every_kind", |b| {
        b.iter(|| scrub_message(black_box(DIRTY)));
    });
    c.bench_function("template_route", |b| {
        b.iter(|| template_route(black_box("https://api.example.com/v1/orders/12345/items/0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e?x=1")));
    });
    c.bench_function("normalize_five_frames", |b| {
        b.iter(|| normalize_frames(black_box(FRAMES)));
    });
}

fn symbolication(c: &mut Criterion) {
    let map = SourceMap::parse(JS_MAP, 1 << 20).expect("map");
    let java = ProguardMap::parse(MAPPING.as_bytes(), 1 << 20).expect("mapping");
    let frames: Vec<Frame> = parse(FRAMES);
    c.bench_function("sourcemap_parse_small_bundle", |b| {
        b.iter(|| SourceMap::parse(black_box(JS_MAP), 1 << 20));
    });
    c.bench_function("sourcemap_lookup_and_function_name", |b| {
        b.iter(|| {
            let found = map.lookup(black_box(1), black_box(56)).expect("mapped");
            map.function_name(&found)
        });
    });
    c.bench_function("proguard_retrace_inlined_frame", |b| {
        b.iter(|| java.retrace(black_box("a.a"), black_box("a"), black_box(16)));
    });
    c.bench_function("fingerprint_five_frames", |b| {
        b.iter(|| fingerprint(black_box("TypeError"), black_box(&frames)));
    });
}

criterion_group!(benches, rules, symbolication);
criterion_main!(benches);
