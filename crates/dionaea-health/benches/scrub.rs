//! What the server pays per health event for the rules in this crate.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dionaea_health::{normalize_frames, scrub_message, template_route};

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

criterion_group!(benches, rules);
criterion_main!(benches);
