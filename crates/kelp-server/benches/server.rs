//! End-to-end benchmarks of the server's hot paths, over loopback QUIC.
//!
//! Every benchmark talks to one [`TestServer`] through a real client
//! connection, so the numbers include framing, TLS and the metadata
//! database, as a user's client would see them.
//!
//! ```text
//! cargo bench -p kelp-server --bench server
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kelp_client::Connection;
use kelp_core::LocalClId;
use kelp_core::{ChunkId, ChunkRef, ContentId, FileManifest};
use kelp_proto::rpc::{FileInfo, SubmitAction, SubmitChange};
use kelp_proto::{Reply, Request};
use kelp_testkit::net::{ADMIN, TestClient, TestServer};
use kelp_testkit::pseudo_random;
use tokio::runtime::Runtime;

const STREAM: &str = "//game/main";
const CHUNK: usize = 64 * 1024;

/// A source of seeds, so that every iteration uploads content the server has
/// not seen. `pseudo_random` gives seeds `2k` and `2k + 1` the same bytes, so
/// seeds step by two.
static SEED: AtomicU64 = AtomicU64::new(2);

fn fresh(len: usize) -> Bytes {
    Bytes::from(pseudo_random(len, SEED.fetch_add(2, Ordering::Relaxed)))
}

fn chunks(count: usize, len: usize) -> Vec<(ChunkId, Bytes)> {
    (0..count)
        .map(|_| {
            let data = fresh(len);
            (ChunkId::of(&data), data)
        })
        .collect()
}

fn add(path: String, manifest: &FileManifest) -> SubmitChange {
    SubmitChange {
        path,
        action: SubmitAction::Add {
            file: FileInfo {
                content: manifest.content,
                size: manifest.size,
                executable: false,
            },
        },
    }
}

fn submit(changes: Vec<SubmitChange>) -> Request {
    Request::Submit {
        stream: STREAM.into(),
        workspace: "bench".into(),
        description: "bench".into(),
        changes,
        integration: None,
        review: None,
        fixes: Vec::new(),
        moves: Vec::new(),
    }
}

async fn put_manifests(conn: &Connection, manifests: Vec<FileManifest>) {
    conn.call(&Request::PutManifests { manifests })
        .await
        .expect("put manifests");
}

struct Fixture {
    rt: Runtime,
    server: TestServer,
    conn: Connection,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let (server, conn) = rt.block_on(async {
            let server = TestServer::start().await;
            server.create_depot("game").await;
            let conn = server.login(ADMIN).await;
            (server, conn)
        });
        Self { rt, server, conn }
    }

    /// Times `f` over `iters` runs, after running `setup` for each outside the
    /// measurement.
    fn timed<S, F, T>(&self, iters: u64, mut setup: S, mut f: F) -> Duration
    where
        S: AsyncFnMut() -> T,
        F: AsyncFnMut(T),
    {
        self.rt.block_on(async {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let input = setup().await;
                let start = Instant::now();
                f(input).await;
                total += start.elapsed();
            }
            total
        })
    }
}

fn requests(c: &mut Criterion) {
    let fx = Fixture::new();
    let conn = &fx.conn;
    let mut g = c.benchmark_group("request");
    g.bench_function("list_streams", |b| {
        b.to_async(&fx.rt)
            .iter(|| async { conn.call(&Request::ListStreams).await.unwrap() })
    });
    g.bench_function("resolve_stream", |b| {
        b.to_async(&fx.rt)
            .iter(|| async { conn.resolve_stream(STREAM, None).await.unwrap() })
    });
    g.bench_function("connect_bearer", |b| {
        let token = fx.rt.block_on(async {
            let issue = Request::IssueToken {
                user: ADMIN.into(),
                ttl_days: 1,
            };
            match conn.call(&issue).await.unwrap() {
                Reply::Token { token, .. } => token,
                other => panic!("unexpected reply {other:?}"),
            }
        });
        b.to_async(&fx.rt).iter(|| async {
            Connection::connect(
                fx.server.addr(),
                kelp_client::Trust::Pinned(fx.server.fingerprint().to_owned()),
                kelp_proto::Auth::Bearer(token.clone()),
            )
            .await
            .unwrap()
        })
    });
    g.finish();
}

fn content(c: &mut Criterion) {
    let fx = Fixture::new();
    let conn = &fx.conn;
    let mut g = c.benchmark_group("content");
    g.sample_size(20);

    for &(count, len) in &[(256usize, 4 * 1024usize), (64, CHUNK), (8, 1024 * 1024)] {
        let id = BenchmarkId::new("put_chunks", format!("{count}x{}KiB", len / 1024));
        g.throughput(Throughput::Bytes((count * len) as u64));
        g.bench_function(id, |b| {
            b.iter_custom(|iters| {
                fx.timed(
                    iters,
                    async || chunks(count, len),
                    async |batch| conn.put_chunks(batch).await.unwrap(),
                )
            })
        });
    }

    let stored = fx.rt.block_on(async {
        let batch = chunks(64, CHUNK);
        let ids: Vec<ChunkId> = batch.iter().map(|(id, _)| *id).collect();
        conn.put_chunks(batch).await.unwrap();
        ids
    });
    g.throughput(Throughput::Bytes((stored.len() * CHUNK) as u64));
    g.bench_function("get_chunks/64x64KiB", |b| {
        b.to_async(&fx.rt)
            .iter(|| async { conn.get_chunks(&stored).await.unwrap() })
    });

    let known: Vec<ChunkId> = (0..4096)
        .map(|i| match stored.get(i) {
            Some(id) => *id,
            None => ChunkId::of(&(i as u64).to_le_bytes()),
        })
        .collect();
    g.throughput(Throughput::Elements(known.len() as u64));
    g.bench_function("has_chunks/4096", |b| {
        b.to_async(&fx.rt).iter(|| async {
            conn.call(&Request::HasChunks { ids: known.clone() })
                .await
                .unwrap()
        })
    });

    // Manifests of several chunks, which the server verifies by rehashing.
    g.throughput(Throughput::Elements(64));
    g.bench_function("put_manifests/64x4chunks", |b| {
        b.iter_custom(|iters| {
            fx.timed(
                iters,
                async || {
                    let mut manifests = Vec::new();
                    let mut all = Vec::new();
                    for _ in 0..64 {
                        let parts = chunks(4, 16 * 1024);
                        let mut hasher = blake3::Hasher::new();
                        for (_, d) in &parts {
                            hasher.update(d);
                        }
                        manifests.push(FileManifest {
                            content: ContentId(kelp_core::Hash(*hasher.finalize().as_bytes())),
                            size: 64 * 1024,
                            chunks: parts
                                .iter()
                                .map(|(id, d)| ChunkRef {
                                    id: *id,
                                    len: d.len() as u32,
                                })
                                .collect(),
                        });
                        all.extend(parts);
                    }
                    conn.put_chunks(all).await.unwrap();
                    manifests
                },
                async |manifests| put_manifests(conn, manifests).await,
            )
        })
    });
    g.finish();
}

fn changes(c: &mut Criterion) {
    let fx = Fixture::new();
    let conn = &fx.conn;
    let mut g = c.benchmark_group("change");
    g.sample_size(20);

    // A stream that already holds many files and some history, as a real
    // depot would: 20,000 files in 100 changelists of 200.
    const BASE: usize = 20_000;
    let base: Vec<FileManifest> = fx.rt.block_on(async {
        let batch = chunks(BASE, 256);
        let manifests: Vec<FileManifest> = batch
            .iter()
            .map(|(id, data)| FileManifest {
                content: ContentId(id.0),
                size: data.len() as u64,
                chunks: vec![ChunkRef {
                    id: *id,
                    len: data.len() as u32,
                }],
            })
            .collect();
        conn.put_chunks(batch).await.unwrap();
        for part in manifests.chunks(1024) {
            put_manifests(conn, part.to_vec()).await;
        }
        for (cl, part) in manifests.chunks(200).enumerate() {
            let changes = part
                .iter()
                .enumerate()
                .map(|(i, m)| add(format!("base/d{}/c{cl}/f{i}.txt", i % 50), m))
                .collect();
            conn.call(&submit(changes)).await.unwrap();
        }
        manifests
    });

    let head = fx.rt.block_on(conn.resolve_stream(STREAM, None)).unwrap();
    let prev = fx
        .rt
        .block_on(conn.resolve_stream(STREAM, Some(head.cl - 1)))
        .unwrap();
    g.throughput(Throughput::Elements(BASE as u64));
    g.bench_function("diff_tree/full_20k", |b| {
        b.to_async(&fx.rt).iter(|| async {
            conn.call(&Request::DiffTree {
                stream: STREAM.into(),
                from: None,
                to: head.root,
                paths: Vec::new(),
            })
            .await
            .unwrap()
        })
    });
    g.throughput(Throughput::Elements(200));
    g.bench_function("diff_tree/one_cl_200", |b| {
        b.to_async(&fx.rt).iter(|| async {
            conn.call(&Request::DiffTree {
                stream: STREAM.into(),
                from: Some(prev.root),
                to: head.root,
                paths: Vec::new(),
            })
            .await
            .unwrap()
        })
    });
    g.throughput(Throughput::Elements(100));
    g.bench_function("changes/100", |b| {
        b.to_async(&fx.rt).iter(|| async {
            conn.call(&Request::Changes {
                stream: Some(STREAM.into()),
                limit: 100,
            })
            .await
            .unwrap()
        })
    });
    g.throughput(Throughput::Elements(1));
    g.bench_function("filelog", |b| {
        b.to_async(&fx.rt).iter(|| async {
            conn.call(&Request::Filelog {
                path: format!("{STREAM}/base/d0/c0/f0.txt"),
            })
            .await
            .unwrap()
        })
    });

    let next = AtomicU64::new(0);
    for &files in &[1usize, 100, 1000] {
        g.throughput(Throughput::Elements(files as u64));
        g.bench_function(BenchmarkId::new("submit_add", files), |b| {
            b.iter_custom(|iters| {
                fx.timed(
                    iters,
                    async || {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        (0..files)
                            .map(|i| add(format!("new/{n}/f{i}.bin"), &base[i % base.len()]))
                            .collect::<Vec<_>>()
                    },
                    async |changes| {
                        conn.call(&submit(changes)).await.unwrap();
                    },
                )
            })
        });
    }

    g.finish();
}

/// Writes `files` small files and one large one into the workspace `name`.
fn populate(client: &TestClient, name: &str, files: usize) {
    for i in 0..files {
        let data = pseudo_random(8 * 1024, SEED.fetch_add(2, Ordering::Relaxed));
        client
            .sandbox()
            .write(&format!("{name}/src/m{}/f{i}.txt", i % 20), data);
    }
    let big = pseudo_random(16 << 20, SEED.fetch_add(2, Ordering::Relaxed));
    client
        .sandbox()
        .write(&format!("{name}/assets/big.bin"), big);
}

/// Whole user operations: a client hashes, uploads and submits files, and
/// another syncs them.
fn workflow(c: &mut Criterion) {
    const FILES: usize = 500;
    let fx = Fixture::new();
    let conn = &fx.conn;
    let stream = kelp_testkit::depot_path(STREAM);
    let mut g = c.benchmark_group("workflow");
    g.sample_size(10);
    g.throughput(Throughput::Bytes((FILES * 8 * 1024 + (16 << 20)) as u64));

    let next = AtomicU64::new(0);
    g.bench_function("reconcile_submit/500x8KiB+16MiB", |b| {
        b.iter_custom(|iters| {
            fx.timed(
                iters,
                async || {
                    let client = TestClient::new();
                    let name = format!("w{}", next.fetch_add(1, Ordering::Relaxed));
                    let mut config = kelp_workspace::Config::new(&name, fx.server.addr(), &stream);
                    config.view = format!("{STREAM}/{name}/... ...");
                    let ws = kelp_client::Workspace::create(&client.sandbox().dir(&name), config)
                        .unwrap();
                    populate(&client, &name, FILES);
                    (client, ws)
                },
                async |(client, mut ws)| {
                    ws.reconcile(conn).await.unwrap();
                    ws.submit(conn, &client.cache(), LocalClId::DEFAULT, Some("b".into()))
                        .await
                        .unwrap();
                },
            )
        })
    });

    // A tree of the same shape for fresh clients to sync.
    fx.rt.block_on(async {
        let client = TestClient::new();
        let mut config = kelp_workspace::Config::new("seed", fx.server.addr(), &stream);
        config.view = format!("{STREAM}/seed/... ...");
        let mut ws = kelp_client::Workspace::create(&client.sandbox().dir("seed"), config).unwrap();
        populate(&client, "seed", FILES);
        ws.reconcile(conn).await.unwrap();
        ws.submit(
            conn,
            &client.cache(),
            LocalClId::DEFAULT,
            Some("seed".into()),
        )
        .await
        .unwrap();
    });
    g.bench_function("sync_fresh/500x8KiB+16MiB", |b| {
        b.iter_custom(|iters| {
            fx.timed(
                iters,
                async || {
                    let client = TestClient::new();
                    let mut config = kelp_workspace::Config::new("ws", fx.server.addr(), &stream);
                    config.view = format!("{STREAM}/seed/... ...");
                    let ws = kelp_client::Workspace::create(&client.sandbox().dir("ws"), config)
                        .unwrap();
                    (client, ws)
                },
                async |(client, mut ws)| {
                    ws.sync(conn, &client.cache(), None).await.unwrap();
                },
            )
        })
    });

    let (_client, ws) = fx.rt.block_on(async {
        let client = TestClient::new();
        let mut config = kelp_workspace::Config::new("ws", fx.server.addr(), &stream);
        config.view = format!("{STREAM}/seed/... ...");
        let mut ws = kelp_client::Workspace::create(&client.sandbox().dir("ws"), config).unwrap();
        ws.sync(conn, &client.cache(), None).await.unwrap();
        (client, ws)
    });
    g.throughput(Throughput::Elements(FILES as u64 + 1));
    g.bench_function("status_clean/501files", |b| b.iter(|| ws.status().unwrap()));
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = requests, content, changes, workflow
}
criterion_main!(benches);
