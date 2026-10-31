//! Benchmarks of the metadata database: submits into a large stream, and the
//! reads behind sync and history.
//!
//! ```text
//! cargo bench -p kelp-meta --bench meta
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kelp_meta::Dirs;
use kelp_testkit::depot_path;
use kelp_testkit::meta::MetaFixture;

const STREAM: &str = "//game/main";
const BASE: usize = 20_000;

/// A stream of [`BASE`] files in 100 changelists.
fn fixture() -> MetaFixture {
    let fx = MetaFixture::new();
    for cl in 0..100 {
        let changes = (0..BASE / 100)
            .map(|i| {
                let path = format!("{STREAM}/base/d{}/c{cl}/f{i}.txt", i % 50);
                fx.add(&path, format!("{cl}/{i}").as_bytes())
            })
            .collect();
        fx.submit_as(STREAM, "alice", "ws", changes, None).unwrap();
    }
    fx
}

fn meta(c: &mut Criterion) {
    let fx = fixture();
    let db = fx.db();
    let stream = depot_path(STREAM);
    let mut g = c.benchmark_group("meta");
    g.sample_size(20);

    let reader = db.read().unwrap();
    let head = reader.stream(&stream).unwrap().unwrap();
    let nodes = reader.nodes().unwrap();
    let dirs = Dirs::new(&nodes);
    g.throughput(Throughput::Elements(BASE as u64));
    g.bench_function("diff_paths/full_20k", |b| {
        b.iter(|| dirs.diff_paths(None, head.root).unwrap())
    });
    let paths: Vec<_> = dirs
        .diff_paths(None, head.root)
        .unwrap()
        .into_iter()
        .map(|d| stream.join(&d.path).unwrap())
        .collect();
    g.bench_function("real_path/20k", |b| {
        b.iter(|| {
            for p in &paths {
                reader.real_path(p).unwrap();
            }
        })
    });
    g.throughput(Throughput::Elements(100));
    g.bench_function("changes/100", |b| {
        b.iter(|| reader.changes(Some(&stream), 100).unwrap())
    });
    g.throughput(Throughput::Elements(1));
    g.bench_function("history", |b| b.iter(|| reader.history(&paths[0]).unwrap()));
    drop(nodes);
    drop(reader);

    let next = AtomicU64::new(0);
    for &files in &[1usize, 100, 1000] {
        g.throughput(Throughput::Elements(files as u64));
        g.bench_function(BenchmarkId::new("submit_add", files), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let changes = (0..files)
                        .map(|i| {
                            let path = format!("{STREAM}/new/{n}/f{i}.bin");
                            fx.add(&path, format!("{n}/{i}").as_bytes())
                        })
                        .collect();
                    let start = Instant::now();
                    fx.submit_as(STREAM, "alice", "ws", changes, None).unwrap();
                    total += start.elapsed();
                }
                total
            })
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = meta
}
criterion_main!(benches);
