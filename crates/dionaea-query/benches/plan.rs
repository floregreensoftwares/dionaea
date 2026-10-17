//! What a report costs before ClickHouse is asked: parsing the request,
//! and validating it against a schema.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dionaea_core::{Schema, parse_schema};
use dionaea_query::{Limits, Query, Site, validate};

const SCHEMA: &str = r#"
[events.checkout.fields.coupon]
kind = "enum"
values = ["save10", "save20"]

[events.checkout.fields.items]
kind = "count"
max = 100

[events.search.fields.term]
kind = "text"
max = 40
"#;

const REQUEST: &str = r#"{
    "from_ms": 1760000000000, "to_ms": 1760604800000,
    "filters": [
        {"dimension": "country", "values": ["BR", "US", "PT"]},
        {"dimension": "platform", "values": ["ios"]}
    ],
    "report": {"type": "series", "measure": "users", "event": "checkout",
               "granularity": "day",
               "breakdown": {"dimension": "property", "event": "checkout", "name": "coupon"}}
}"#;

fn plan(c: &mut Criterion) {
    let schemas: Vec<Schema> = vec![parse_schema(SCHEMA).expect("schema")];
    let site = Site {
        id: 7,
        time_zone: "America/Sao_Paulo",
        schemas: &schemas,
    };
    let limits = Limits::default();
    let query: Query = serde_json::from_str(REQUEST).expect("request");
    c.bench_function("parse_request", |b| {
        b.iter(|| serde_json::from_str::<Query>(black_box(REQUEST)));
    });
    c.bench_function("validate", |b| {
        b.iter(|| validate(black_box(site), black_box(&query), black_box(&limits)));
    });
}

criterion_group!(benches, plan);
criterion_main!(benches);
