//! What a login costs: hashing and verifying a password at the default
//! argon2id parameters, the secret generation and the lookup hash of a
//! session or token, and checking the cookie header.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use dionaea_auth::{Hasher, SESSION_PREFIX, cookie_from_header, generate, has_shape, hash};

fn password(c: &mut Criterion) {
    let hasher = Hasher::new().expect("default cost");
    let stored = hasher.hash("correct horse battery").expect("hash");
    let mut group = c.benchmark_group("password");
    group.sample_size(20);
    group.bench_function("hash_default_cost", |b| {
        b.iter(|| hasher.hash(black_box("correct horse battery")));
    });
    group.bench_function("verify_default_cost", |b| {
        b.iter(|| hasher.verify(black_box("correct horse battery"), black_box(&stored)));
    });
    group.finish();
}

fn secrets(c: &mut Criterion) {
    let secret = generate(SESSION_PREFIX).expect("secret");
    let header = format!("theme=dark; dionaea_session={}; lang=en", secret.value);
    c.bench_function("session_secret_generate", |b| {
        b.iter(|| generate(black_box(SESSION_PREFIX)));
    });
    c.bench_function("session_secret_hash", |b| {
        b.iter(|| hash(black_box(&secret.value)));
    });
    c.bench_function("session_cookie_to_shape_check", |b| {
        b.iter(|| {
            cookie_from_header(black_box(&header))
                .is_some_and(|value| has_shape(SESSION_PREFIX, value))
        });
    });
}

criterion_group!(benches, password, secrets);
criterion_main!(benches);
