//! What the ingest path pays to follow the metadata: the generation poll
//! that runs every few seconds, and the full snapshot reload that runs only
//! when it changed.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dionaea_meta::{MetaStore, SqliteMeta};

const SCHEMA: &str = r#"
[events.checkout_started.fields.items]
kind = "count"
max = 100
required = true

[events.checkout_started.fields.coupon]
kind = "text"
max = 32

[events.checkout_started.fields.region]
kind = "enum"
values = ["us-east", "eu-west"]

[events.screen_opened.fields.name]
kind = "id"
required = true
"#;

fn populated(sites: usize, keys_per_site: usize) -> SqliteMeta {
    let store = SqliteMeta::open_in_memory().expect("database");
    let organization = store.create_organization("bench").expect("organization");
    for n in 0..sites {
        let site = store
            .create_site(organization.id, &format!("site-{n}"))
            .expect("site");
        store.publish_schema(site.id, SCHEMA).expect("schema");
        for k in 0..keys_per_site {
            store
                .issue_write_key(
                    site.id,
                    &format!("key-{k}"),
                    &["https://app.example".to_owned()],
                )
                .expect("key");
        }
    }
    store
}

fn benches(c: &mut Criterion) {
    let small = populated(1, 2);
    c.bench_function("generation", |b| {
        b.iter(|| black_box(small.generation().expect("generation")));
    });
    c.bench_function("snapshot 1 site, 2 keys", |b| {
        b.iter(|| black_box(small.snapshot().expect("snapshot")));
    });
    let large = populated(100, 10);
    c.bench_function("snapshot 100 sites, 1000 keys", |b| {
        b.iter(|| black_box(large.snapshot().expect("snapshot")));
    });
}

criterion_group!(group, benches);
criterion_main!(group);
