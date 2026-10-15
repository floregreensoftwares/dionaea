//! `schema diff` runs on push and in review, off the ingest path; these
//! numbers show it stays cheap for large schemas, and that comparing
//! unchanged schemas costs little.

use std::fmt::Write;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dionaea_core::{Schema, diff, parse_schema};

/// `events` events with 16 fields each; `max` is the `count` limit of the
/// last field, so two files differ by one narrowed or widened value.
fn schema(events: usize, max: u64) -> Schema {
    let mut source = String::new();
    for event in 0..events {
        for field in 0..15 {
            let _ = writeln!(
                source,
                "[events.event_{event}.fields.f{field}]\nkind = \"id\"\n"
            );
        }
        let _ = writeln!(
            source,
            "[events.event_{event}.fields.last]\nkind = \"count\"\nmax = {max}\n"
        );
    }
    parse_schema(&source).expect("the benchmark schema is valid")
}

fn diff_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff");
    for events in [20, 200] {
        let old = schema(events, 100);
        let same = schema(events, 100);
        let narrowed = schema(events, 50);
        group.bench_function(format!("{events} events x 16 fields, unchanged"), |b| {
            b.iter(|| diff(black_box(&old), black_box(&same)));
        });
        group.bench_function(
            format!("{events} events x 16 fields, one change per event"),
            |b| b.iter(|| diff(black_box(&old), black_box(&narrowed))),
        );
    }
    group.finish();
}

criterion_group!(benches, diff_benches);
criterion_main!(benches);
