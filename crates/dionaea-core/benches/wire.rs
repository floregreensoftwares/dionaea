//! Every collect request is decoded before the guard sees an event, so this
//! is the first cost on the ingest path.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dionaea_core::{decode_batch, decode_identify};
use serde_json::{Value, json};

fn event(index: usize) -> Value {
    json!({
        "event_id": format!("0190f7e2-7c3e-7a52-9a0e-{index:012x}"),
        "name": "checkout_started",
        "ts": 1_760_000_000_000u64 + index as u64,
        "anon_id": "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e",
        "session_id": "0190f7e2-7c3e-7a52-9a0e-aaaaaaaaaaaa",
        "screen": "/orders/:id",
        "props": { "items": 3, "coupon": "SPRING SALE", "region": "us-east", "progress": 0.42 }
    })
}

fn body(events: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "sent_at": 1_760_000_001_000u64, "sdk": "dionaea-rn/0.1.0", "release": "1.4.2+57",
        "events": events
    }))
    .expect("serializable")
}

fn wire(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_batch");
    for count in [1, 20, 100] {
        let bytes = body((0..count).map(event).collect());
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(format!("{count} events ({} bytes)", bytes.len()), |b| {
            b.iter(|| decode_batch(black_box(&bytes)));
        });
    }

    let mixed = body(
        (0..100)
            .map(|i| {
                if i % 5 == 0 {
                    json!("not an object")
                } else {
                    event(i)
                }
            })
            .collect(),
    );
    group.throughput(Throughput::Elements(100));
    group.bench_function("100 events, 20 malformed", |b| {
        b.iter(|| decode_batch(black_box(&mixed)));
    });

    let oversized = vec![b' '; 256 * 1024 + 1];
    group.throughput(Throughput::Elements(1));
    group.bench_function("reject: body over the limit", |b| {
        b.iter(|| decode_batch(black_box(&oversized)));
    });

    let nested = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    group.bench_function("reject: 10000 levels of nesting", |b| {
        b.iter(|| decode_batch(black_box(nested.as_bytes())));
    });

    // The shapes that fit the body limit and cost the most to read.
    let mut tiny_props = String::from("{");
    let mut index = 0;
    while tiny_props.len() < 256 * 1024 - 400 {
        tiny_props.push_str(&format!("\"k{index}\":1,"));
        index += 1;
    }
    tiny_props.push_str("\"last\":1}");
    let one_event_many_props = format!(
        "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{{\"event_id\":\"0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e\",\"name\":\"e\",\"ts\":1,\"anon_id\":\"a\",\"props\":{tiny_props}}}]}}"
    );
    group.throughput(Throughput::Bytes(one_event_many_props.len() as u64));
    group.bench_function("worst case: one event, 256 KiB of tiny properties", |b| {
        b.iter(|| decode_batch(black_box(one_event_many_props.as_bytes())));
    });

    let many_scalars = format!(
        "{{\"sent_at\":1,\"sdk\":\"s\",\"events\":[{}1]}}",
        "1,".repeat(120_000)
    );
    group.throughput(Throughput::Bytes(many_scalars.len() as u64));
    group.bench_function("worst case: 120000 tiny array elements, refused", |b| {
        b.iter(|| decode_batch(black_box(many_scalars.as_bytes())));
    });
    group.finish();

    let identify = serde_json::to_vec(&json!({
        "sent_at": 1_760_000_001_000u64, "sdk": "dionaea-web/0.1.0",
        "anon_id": "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e", "user_id": "u_42",
        "traits": { "plan": "pro", "seats": 25 }
    }))
    .expect("serializable");
    c.bench_function("decode_identify", |b| {
        b.iter(|| decode_identify(black_box(&identify)));
    });
}

criterion_group!(benches, wire);
criterion_main!(benches);
