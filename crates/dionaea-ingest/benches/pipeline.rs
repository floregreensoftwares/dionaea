//! The per-request cost of the ingest path, without HTTP or disk: admission,
//! the rate limiter and a whole batch through decode, guard and mapping.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dionaea_ingest::{Admission, Cache, Limits, RateLimiter, process};
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

[events.checkout_started.fields.progress]
kind = "unit"
"#;

const NOW: u64 = 1_760_000_000_000;

fn admission() -> (Cache, String, Admission) {
    let store = SqliteMeta::open_in_memory().expect("store");
    let organization = store.create_organization("bench").expect("organization");
    let site = store.create_site(organization.id, "site").expect("site");
    store.publish_schema(site.id, SCHEMA).expect("schema");
    let key = store
        .issue_write_key(site.id, "k", &["https://app.example".to_owned()])
        .expect("key")
        .key;
    let cache = Cache::new();
    cache.refresh(&store).expect("refresh");
    let admission = cache
        .admit(&key, Some("https://app.example"))
        .expect("admitted");
    (cache, key, admission)
}

fn batch(events: usize, valid: bool) -> Vec<u8> {
    let mut body =
        format!("{{\"sent_at\":{NOW},\"sdk\":\"bench/1.0\",\"release\":\"1.4.2\",\"events\":[");
    for n in 0..events {
        if n > 0 {
            body.push(',');
        }
        let name = if valid {
            "checkout_started"
        } else {
            "unknown_event"
        };
        write!(
            body,
            "{{\"event_id\":\"01890000-0000-7000-8000-{n:012}\",\"ts\":{},\"name\":\"{name}\",\
             \"anon_id\":\"anon-{n}\",\"session_id\":\"session-1\",\"screen\":\"/cart\",\
             \"props\":{{\"items\":3,\"coupon\":\"SPRING\",\"region\":\"eu-west\",\"progress\":0.5,\
             \"extra\":\"stripped\"}}}}",
            NOW - 1000
        )
        .expect("write to a string");
    }
    body.push_str("]}");
    body.into_bytes()
}

fn benches(c: &mut Criterion) {
    let (cache, key, admission) = admission();
    c.bench_function("admit", |b| {
        b.iter(|| black_box(cache.admit(black_box(&key), Some("https://app.example"))));
    });

    let limiter = RateLimiter::new(1_000_000.0, 1_000_000.0, 10_000);
    let mut now = 0u64;
    c.bench_function("rate limit check", |b| {
        b.iter(|| {
            now += 1;
            black_box(limiter.check(black_box(7u32), now))
        });
    });

    let limits = Limits::default();
    let mut group = c.benchmark_group("process");
    for (events, valid, label) in [
        (1, true, "1 event"),
        (100, true, "100 events"),
        (100, false, "100 events, all dropped"),
    ] {
        let body = batch(events, valid);
        group.throughput(Throughput::Elements(events as u64));
        group.bench_function(label, |b| {
            b.iter(|| black_box(process(&admission, black_box(&body), NOW, &limits)));
        });
    }
    group.finish();
}

criterion_group!(group, benches);
criterion_main!(group);
