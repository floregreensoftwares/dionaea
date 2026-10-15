//! Schema files are parsed on push and when a node refreshes its cache, so
//! parsing is off the hot path; these numbers bound how large a schema can
//! be before that stops being true.

use std::fmt::Write;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dionaea_core::parse_schema;

/// `events` events with `fields` fields each, mixing every kind.
fn schema_file(events: usize, fields: usize) -> String {
    let kinds = [
        "kind = \"id\"",
        "kind = \"text\"\nmax = 64",
        "kind = \"unit\"",
        "kind = \"enum\"\nvalues = [\"a\", \"b\", \"c\"]",
        "kind = \"count\"\nmax = 1000",
        "kind = \"duration\"\nmax = 60000",
    ];
    let mut source = String::new();
    for event in 0..events {
        for field in 0..fields {
            let kind = kinds[field % kinds.len()];
            let _ = writeln!(
                source,
                "[events.event_{event}.fields.field_{field}]\n{kind}\n"
            );
        }
    }
    source
}

fn parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_schema");
    for (events, fields) in [(1, 6), (20, 8), (200, 16)] {
        let source = schema_file(events, fields);
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_function(format!("{events} events x {fields} fields"), |b| {
            b.iter(|| parse_schema(black_box(&source)).expect("valid"));
        });
    }

    let mut broken = String::new();
    for i in 0..2000 {
        let _ = writeln!(broken, "[traits.t{i}]\nkind = \"nope\"\n");
    }
    group.throughput(Throughput::Bytes(broken.len() as u64));
    group.bench_function("2000 errors, 100 kept", |b| {
        b.iter(|| parse_schema(black_box(&broken)).expect_err("invalid"));
    });
    group.finish();
}

criterion_group!(benches, parse);
criterion_main!(benches);
