//! Benchmarks of building, editing, reading and diffing prolly trees.
//!
//! ```text
//! cargo bench -p kelp-prolly --bench prolly
//! ```

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kelp_prolly::{ChunkConfig, MemStore, Prolly};

/// Entries shaped like a depot tree: a path and a 48-byte file record.
fn entries(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n)
        .map(|i| {
            let key = format!("Content/Maps/m{:03}/Props/asset_{i:07}.uasset", i % 300);
            (key.into_bytes(), kelp_testkit::pseudo_random(48, i as u64))
        })
        .collect()
}

fn prolly(c: &mut Criterion) {
    let mut g = c.benchmark_group("prolly");
    g.sample_size(10);
    for &n in &[10_000usize, 200_000] {
        let mut all = entries(n);
        all.sort();
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(BenchmarkId::new("build", n), |b| {
            b.iter(|| {
                let store = MemStore::default();
                Prolly::new(&store, ChunkConfig::DEFAULT)
                    .build(all.iter().cloned())
                    .unwrap()
            })
        });

        let store = MemStore::default();
        let tree = Prolly::new(&store, ChunkConfig::DEFAULT);
        let root = tree.build(all.iter().cloned()).unwrap();
        // A typical submit: a hundred files changed across the tree.
        let edits: Vec<_> = (0..100)
            .map(|i| {
                let (k, _) = &all[i * n / 100];
                (k.clone(), Some(vec![i as u8; 48]))
            })
            .collect();
        g.throughput(Throughput::Elements(edits.len() as u64));
        g.bench_function(BenchmarkId::new("apply_100", n), |b| {
            b.iter(|| tree.apply(root, edits.iter().cloned()).unwrap())
        });
        let edited = tree.apply(root, edits.iter().cloned()).unwrap();
        g.bench_function(BenchmarkId::new("diff_100", n), |b| {
            b.iter(|| tree.diff(root, edited).unwrap())
        });
        g.bench_function(BenchmarkId::new("get_100", n), |b| {
            b.iter(|| {
                for (k, _) in &edits {
                    tree.get(root, k).unwrap();
                }
            })
        });
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(BenchmarkId::new("entries", n), |b| {
            b.iter(|| tree.entries(root).unwrap())
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = prolly
}
criterion_main!(benches);
