use bytes::Bytes;
use kelp_cas::{BlobStore, Error, FsBlobStore};
use kelp_core::ChunkId;
use kelp_testkit::Sandbox;

#[tokio::test]
async fn round_trip() {
    let sandbox = Sandbox::new();
    let store = FsBlobStore::new(sandbox.dir("chunks"));
    let data = Bytes::from_static(b"hello kelp");
    let id = ChunkId::of(&data);

    assert_eq!(store.has(&[id]).await.unwrap(), vec![false]);
    store.put(id, data.clone()).await.unwrap();
    store.put(id, data.clone()).await.unwrap();
    assert_eq!(store.has(&[id]).await.unwrap(), vec![true]);
    assert_eq!(store.get(id).await.unwrap(), data);

    store.delete(id).await.unwrap();
    assert!(matches!(store.get(id).await, Err(Error::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_of_the_same_chunk_all_succeed() {
    // Identical files ingested in parallel store the same chunks at once.
    let sandbox = Sandbox::new();
    for store in [
        FsBlobStore::new(sandbox.dir("durable")),
        FsBlobStore::cache(sandbox.dir("cache")),
    ] {
        for round in 0..20u8 {
            let data = Bytes::from(vec![round; 8 * 1024]);
            let id = ChunkId::of(&data);
            let mut puts = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let (store, data) = (store.clone(), data.clone());
                puts.spawn(async move { store.put(id, data).await });
            }
            while let Some(put) = puts.join_next().await {
                put.unwrap().unwrap();
            }
            assert_eq!(store.get(id).await.unwrap(), data);
        }
    }
}
