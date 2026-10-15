//! The guard runs once per incoming event, so its cost sets the ceiling of
//! the ingest path. Each case states what it models.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dionaea_core::{Schema, parse_schema};
use serde_json::{Map, Value, json};

const SCHEMA: &str = r#"
[events.checkout_started.fields.items]
kind = "count"
max = 1000
required = true

[events.checkout_started.fields.coupon]
kind = "text"
max = 64

[events.checkout_started.fields.region]
kind = "enum"
values = ["us-east", "us-west", "eu-west", "eu-central", "ap-south"]

[events.checkout_started.fields.progress]
kind = "unit"

[events.checkout_started.fields.load_time]
kind = "duration"
max = 600000

[events.checkout_started.fields.cart]
kind = "id"
required = true

[traits.plan]
kind = "enum"
values = ["free", "pro", "team"]

[traits.seats]
kind = "count"
max = 10000
"#;

fn schema() -> Schema {
    parse_schema(SCHEMA).expect("the benchmark schema is valid")
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("an object").clone()
}

fn guard(c: &mut Criterion) {
    let schema = schema();
    let mut group = c.benchmark_group("guard_event");
    group.throughput(Throughput::Elements(1));

    let typical = object(json!({
        "items": 3, "coupon": "SPRING SALE", "region": "us-east",
        "progress": 0.4237, "load_time": 1840, "cart": "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e"
    }));
    group.bench_function("typical six declared fields", |b| {
        b.iter(|| schema.guard_event(black_box("checkout_started"), black_box(&typical)));
    });

    let mut noisy = typical.clone();
    for i in 0..20 {
        noisy.insert(format!("extra_{i}"), json!("user supplied value"));
    }
    group.bench_function("six declared plus twenty undeclared", |b| {
        b.iter(|| schema.guard_event(black_box("checkout_started"), black_box(&noisy)));
    });

    let long_text = object(json!({
        "items": 1, "cart": "c1", "coupon": "x".repeat(200_000)
    }));
    group.bench_function("text field of 200 KB cut at 64", |b| {
        b.iter(|| schema.guard_event(black_box("checkout_started"), black_box(&long_text)));
    });

    let tap = object(json!({ "screen": "home", "element": "buy_button" }));
    group.bench_function("built-in ui_tap", |b| {
        b.iter(|| schema.guard_event(black_box("ui_tap"), black_box(&tap)));
    });

    let missing = object(json!({ "coupon": "SPRING" }));
    group.bench_function("drop: missing required field", |b| {
        b.iter(|| schema.guard_event(black_box("checkout_started"), black_box(&missing)));
    });

    group.bench_function("drop: unknown event", |b| {
        b.iter(|| schema.guard_event(black_box("not_declared"), black_box(&typical)));
    });

    let huge_name = "a".repeat(200_000);
    group.bench_function("drop: 200 KB event name", |b| {
        b.iter(|| schema.guard_event(black_box(&huge_name), black_box(&typical)));
    });

    let at_limit: Map<String, Value> = (0..32).map(|i| (format!("k{i}"), json!(i))).collect();
    let over_limit: Map<String, Value> = (0..33).map(|i| (format!("k{i}"), json!(i))).collect();
    group.bench_function("32 properties", |b| {
        b.iter(|| schema.guard_event(black_box("checkout_started"), black_box(&at_limit)));
    });
    group.bench_function("drop: 33 properties", |b| {
        b.iter(|| schema.guard_event(black_box("checkout_started"), black_box(&over_limit)));
    });
    group.finish();

    let traits = object(json!({ "plan": "pro", "seats": 25, "email": "a@b.c" }));
    c.bench_function("guard_traits", |b| {
        b.iter(|| schema.guard_traits(black_box(&traits)));
    });
}

criterion_group!(benches, guard);
criterion_main!(benches);
