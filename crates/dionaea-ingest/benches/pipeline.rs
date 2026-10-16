//! The per-request cost of the ingest path, without HTTP or disk: admission,
//! the rate limiter and a whole batch through decode, guard and mapping.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dionaea_ingest::{
    Admission, Cache, Deduper, Limits, RateLimiter, enqueue, parse_user_agent, process,
};
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
    let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
    c.bench_function("user agent: a browser", |b| {
        b.iter(|| black_box(parse_user_agent(black_box(chrome))));
    });
    c.bench_function("user agent: a crawler", |b| {
        b.iter(|| {
            black_box(parse_user_agent(black_box(
                "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            )))
        });
    });
    let seen = Deduper::new(1_000_000, 600_000);
    seen.remember(0..500_000u128, 0);
    c.bench_function("duplicate check", |b| {
        b.iter(|| black_box(seen.contains(black_box(250_000), 1_000)));
    });
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

    // Encoding a request into a spool record and appending it, without fsync
    // (the spool benchmark measures that part).
    let dir = std::env::temp_dir().join(format!("dionaea-ingest-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = dionaea_spool::Config::new(&dir);
    config.sync = false;
    config.quota_bytes = 8 << 30;
    let spool = dionaea_spool::Spool::open(config).expect("spool");
    let processed = process(&admission, &batch(100, true), NOW, &limits).expect("batch");
    let mut group = c.benchmark_group("enqueue");
    group.throughput(Throughput::Elements(100));
    group.bench_function("100 events, no fsync", |b| {
        b.iter(|| black_box(enqueue(&spool, black_box(&processed), 1, NOW)));
    });
    group.finish();
    drop(spool);
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(group, benches);
criterion_main!(group);
