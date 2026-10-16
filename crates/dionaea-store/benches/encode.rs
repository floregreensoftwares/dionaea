//! Turning accepted events into `JSONEachRow` text happens once per event on
//! the way to ClickHouse; this measures it apart from the network.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dionaea_store::{EventRow, Store};

fn event(n: u64) -> EventRow {
    EventRow {
        site_id: 7,
        event_id: format!("0190f7e2-7c3e-7a52-9a0e-{n:012x}"),
        ts_ms: 1_760_000_000_000 + n,
        received_ms: 1_760_000_001_000,
        name: "checkout_started".to_owned(),
        schema_version: 3,
        anon_id: format!("anon-{n}"),
        session_id: "0190f7e2-7c3e-7a52-9a0e-aaaaaaaaaaaa".to_owned(),
        screen: "/orders/:id".to_owned(),
        props_str: vec![
            ("coupon".to_owned(), "SPRING SALE".to_owned()),
            ("region".to_owned(), "us-east".to_owned()),
        ],
        props_num: vec![("items".to_owned(), 3.0), ("progress".to_owned(), 0.4237)],
        country: "BR".to_owned(),
        platform: "ios".to_owned(),
        os: "iOS 18".to_owned(),
        browser: String::new(),
        device_class: "phone".to_owned(),
        app_version: "1.4.2+57".to_owned(),
        sdk: "dionaea-rn/0.1.0".to_owned(),
    }
}

fn encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode_events");
    for count in [1usize, 100, 10_000] {
        let rows: Vec<EventRow> = (0..count as u64).map(event).collect();
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(format!("{count} events"), |b| {
            b.iter(|| Store::encode_events(black_box(&rows)));
        });
    }
    group.finish();
}

criterion_group!(benches, encode);
criterion_main!(benches);
