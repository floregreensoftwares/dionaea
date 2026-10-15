//! The spool sits on every accepted event: a request is acknowledged only after
//! its record is durable. What matters is how many appends per second the disk
//! allows with real `fsync` (and how much group commit helps when requests
//! overlap), and what the spool itself costs without the disk.

use std::path::PathBuf;
use std::thread;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use dionaea_spool::{Config, Spool};

/// A directory under the system temp folder, removed afterwards.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("dionaea-spool-bench-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &Dir, sync: bool) -> Config {
    let mut config = Config::new(&dir.0);
    config.sync = sync;
    config.quota_bytes = 64 * 1024 * 1024 * 1024;
    config
}

/// Appends `total` records of `payload` from `threads` threads.
fn append_from(spool: &Spool, threads: usize, total: usize, payload: &[u8]) {
    let per_thread = total / threads;
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..per_thread {
                    spool.append(payload).expect("append");
                }
            });
        }
    });
}

fn appends(c: &mut Criterion) {
    let payload = vec![7u8; 400];

    let mut group = c.benchmark_group("append with fsync");
    group.sample_size(20);
    for threads in [1usize, 8, 64] {
        let total = if threads == 1 { 64 } else { 512 };
        let dir = Dir::new(&format!("sync{threads}"));
        let spool = Spool::open(config(&dir, true)).expect("open");
        group.throughput(Throughput::Elements(total as u64));
        group.bench_function(format!("{threads} threads, {total} records"), |b| {
            b.iter(|| append_from(&spool, threads, total, &payload));
        });
        let stats = spool.stats();
        eprintln!(
            "  {threads} threads: {} records over {} fsyncs ({:.1} records per fsync)",
            stats.appended_records,
            stats.syncs,
            stats.appended_records as f64 / stats.syncs.max(1) as f64
        );
    }
    group.finish();

    let mut group = c.benchmark_group("append without fsync");
    for threads in [1usize, 8] {
        let total = 4096;
        let dir = Dir::new(&format!("nosync{threads}"));
        let spool = Spool::open(config(&dir, false)).expect("open");
        group.throughput(Throughput::Elements(total as u64));
        group.bench_function(format!("{threads} threads, {total} records"), |b| {
            b.iter(|| append_from(&spool, threads, total, &payload));
        });
    }
    group.finish();
}

fn reads(c: &mut Criterion) {
    let payload = vec![9u8; 1000];
    let records = 20_000usize;
    let dir = Dir::new("read");
    let spool = Spool::open(config(&dir, false)).expect("open");
    for _ in 0..records {
        spool.append(payload.as_slice()).expect("append");
    }

    let mut group = c.benchmark_group("read and commit");
    group.throughput(Throughput::Elements(records as u64));
    group.bench_function("read 20000 records in batches of 500", |b| {
        b.iter_batched(
            || spool.rewind(),
            |()| {
                let mut seen = 0;
                loop {
                    let batch = spool.read(500, usize::MAX).expect("read");
                    if batch.is_empty() {
                        break;
                    }
                    seen += batch.len();
                }
                assert_eq!(seen, records);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();

    // Committing costs a flushed marker file per batch.
    let dir = Dir::new("commit");
    let mut with_sync = config(&dir, true);
    with_sync.segment_bytes = 1024 * 1024;
    let spool = Spool::open(with_sync).expect("open");
    for _ in 0..2_000 {
        spool.append(payload.as_slice()).expect("append");
    }
    let mut group = c.benchmark_group("commit");
    group.sample_size(20);
    group.bench_function("read 50 and commit, marker flushed", |b| {
        b.iter(|| {
            let batch = spool.read(50, usize::MAX).expect("read");
            if let Some(last) = batch.last() {
                spool.commit(last.next).expect("commit");
            } else {
                spool.rewind();
            }
        });
    });
    group.finish();
}

criterion_group!(benches, appends, reads);
criterion_main!(benches);
