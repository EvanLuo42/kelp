//! Benchmarks of chunking and of the chunk stores.
//!
//! ```text
//! cargo bench -p kelp-cas --bench cas
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kelp_cas::{BlobStore, Chunker, FastCdcChunker, FsBlobStore, PackStore, build_manifest};
use kelp_core::ChunkId;
use kelp_testkit::{Sandbox, pseudo_random};
use tokio::runtime::Runtime;

/// `pseudo_random` gives seeds `2k` and `2k + 1` the same bytes, so seeds step by two.
static SEED: AtomicU64 = AtomicU64::new(2);

fn chunks(count: usize, len: usize) -> Vec<(ChunkId, Bytes)> {
    (0..count)
        .map(|_| {
            let data = Bytes::from(pseudo_random(len, SEED.fetch_add(2, Ordering::Relaxed)));
            (ChunkId::of(&data), data)
        })
        .collect()
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn chunking(c: &mut Criterion) {
    let mut g = c.benchmark_group("chunking");
    g.sample_size(10);
    let chunker = FastCdcChunker::default();
    for &len in &[1usize << 20, 64 << 20] {
        let data = pseudo_random(len, 7);
        g.throughput(Throughput::Bytes(len as u64));
        let size = format!("{}MiB", len >> 20);
        g.bench_function(BenchmarkId::new("fastcdc", &size), |b| {
            b.iter(|| chunker.chunk(&data))
        });
        g.bench_function(BenchmarkId::new("build_manifest", &size), |b| {
            b.iter(|| build_manifest(&data, &chunker))
        });
        g.bench_function(BenchmarkId::new("blake3", &size), |b| {
            b.iter(|| blake3::hash(&data))
        });
    }
    g.finish();
}

/// Times `f` on fresh input from `setup`, excluding the setup.
fn timed<T>(
    rt: &Runtime,
    iters: u64,
    mut setup: impl FnMut() -> T,
    mut f: impl AsyncFnMut(T),
) -> Duration {
    rt.block_on(async {
        let mut total = Duration::ZERO;
        for _ in 0..iters {
            let input = setup();
            let start = Instant::now();
            f(input).await;
            total += start.elapsed();
        }
        total
    })
}

fn pack_store(c: &mut Criterion) {
    let rt = runtime();
    let sandbox = Sandbox::new();
    let store = PackStore::open(sandbox.dir("packs")).unwrap();
    let mut g = c.benchmark_group("pack_store");
    g.sample_size(20);

    for &(count, len) in &[(1024usize, 4 * 1024usize), (256, 64 * 1024), (8, 1 << 20)] {
        g.throughput(Throughput::Bytes((count * len) as u64));
        let id = BenchmarkId::new("put_sync", format!("{count}x{}KiB", len >> 10));
        g.bench_function(id, |b| {
            b.iter_custom(|iters| {
                timed(
                    &rt,
                    iters,
                    || chunks(count, len),
                    async |batch| {
                        let mut tasks: tokio::task::JoinSet<kelp_cas::Result<()>> =
                            tokio::task::JoinSet::new();
                        for (id, data) in batch {
                            let store = store.clone();
                            tasks.spawn(async move { store.put(id, data).await });
                        }
                        while let Some(r) = tasks.join_next().await {
                            r.unwrap().unwrap();
                        }
                        store.sync().await.unwrap();
                    },
                )
            })
        });
    }

    let stored = rt.block_on(async {
        let batch = chunks(256, 64 * 1024);
        for (id, data) in &batch {
            store.put(*id, data.clone()).await.unwrap();
        }
        store.sync().await.unwrap();
        batch.into_iter().map(|(id, _)| id).collect::<Vec<_>>()
    });
    g.throughput(Throughput::Bytes((stored.len() * 64 * 1024) as u64));
    g.bench_function("get/256x64KiB", |b| {
        b.to_async(&rt).iter(|| async {
            for id in &stored {
                store.get(*id).await.unwrap();
            }
        })
    });
    let probe: Vec<ChunkId> = (0..4096u64)
        .map(|i| match stored.get(i as usize) {
            Some(id) => *id,
            None => ChunkId::of(&i.to_le_bytes()),
        })
        .collect();
    g.throughput(Throughput::Elements(probe.len() as u64));
    g.bench_function("has/4096", |b| {
        b.to_async(&rt)
            .iter(|| async { store.has(&probe).await.unwrap() })
    });
    g.finish();
}

fn loose_store(c: &mut Criterion) {
    let rt = runtime();
    let sandbox = Sandbox::new();
    let store = FsBlobStore::new(sandbox.dir("loose"));
    let mut g = c.benchmark_group("fs_store");
    g.sample_size(10);
    g.throughput(Throughput::Bytes(16 << 20));
    g.bench_function("put_sync/16x1MiB", |b| {
        b.iter_custom(|iters| {
            timed(
                &rt,
                iters,
                || chunks(16, 1 << 20),
                async |batch| {
                    for (id, data) in batch {
                        store.put(id, data).await.unwrap();
                    }
                    store.sync().await.unwrap();
                },
            )
        })
    });
    g.finish();

    // The client's cache: no syncs, many small files.
    let cache = FsBlobStore::cache(sandbox.dir("cache"));
    let mut g = c.benchmark_group("cache_store");
    g.sample_size(10);
    g.throughput(Throughput::Elements(500));
    for parallel in [1usize, 16] {
        g.bench_function(BenchmarkId::new("put/500x8KiB", parallel), |b| {
            b.iter_custom(|iters| {
                timed(
                    &rt,
                    iters,
                    || chunks(500, 8 * 1024),
                    async |batch| {
                        let mut tasks: tokio::task::JoinSet<kelp_cas::Result<()>> =
                            tokio::task::JoinSet::new();
                        for (id, data) in batch {
                            if tasks.len() >= parallel {
                                tasks.join_next().await.unwrap().unwrap().unwrap();
                            }
                            let cache = cache.clone();
                            tasks.spawn(async move { cache.put(id, data).await });
                        }
                        while let Some(r) = tasks.join_next().await {
                            r.unwrap().unwrap();
                        }
                    },
                )
            })
        });
    }
    let stored = rt.block_on(async {
        let batch = chunks(500, 8 * 1024);
        for (id, data) in &batch {
            cache.put(*id, data.clone()).await.unwrap();
        }
        batch.into_iter().map(|(id, _)| id).collect::<Vec<_>>()
    });
    for parallel in [1usize, 16] {
        g.bench_function(BenchmarkId::new("get/500x8KiB", parallel), |b| {
            b.to_async(&rt).iter(|| async {
                let mut tasks: tokio::task::JoinSet<kelp_cas::Result<Bytes>> =
                    tokio::task::JoinSet::new();
                for id in stored.iter().copied() {
                    if tasks.len() >= parallel {
                        tasks.join_next().await.unwrap().unwrap().unwrap();
                    }
                    let cache = cache.clone();
                    tasks.spawn(async move { cache.get(id).await });
                }
                while let Some(r) = tasks.join_next().await {
                    r.unwrap().unwrap();
                }
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
    targets = chunking, pack_store, loose_store
}
criterion_main!(benches);
