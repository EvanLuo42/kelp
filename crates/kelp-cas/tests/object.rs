use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use kelp_cas::{BlobStore, ObjectBlobStore, PackStore, TieredStore};
use kelp_core::ChunkId;
use kelp_testkit::{Sandbox, pseudo_random};
use object_store::memory::InMemory;

fn chunk(seed: u64, len: usize) -> (ChunkId, Bytes) {
    let data = pseudo_random(len, seed);
    (ChunkId::of(&data), data.into())
}

async fn exercise(store: &ObjectBlobStore) {
    let chunks: Vec<_> = (0..20).map(|i| chunk(i, 1000 + i as usize * 100)).collect();
    for (id, data) in &chunks {
        store.put(*id, data.clone()).await.unwrap();
    }
    for (id, data) in &chunks {
        assert_eq!(&store.get(*id).await.unwrap(), data);
    }
    let missing = ChunkId::of(b"missing");
    assert_eq!(
        store.has(&[chunks[3].0, missing]).await.unwrap(),
        [true, false]
    );
    assert!(matches!(
        store.get(missing).await,
        Err(kelp_cas::Error::NotFound(_))
    ));
    let ids: HashSet<_> = store.ids().await.unwrap().into_iter().collect();
    assert_eq!(ids, chunks.iter().map(|(id, _)| *id).collect());
    store.delete(chunks[0].0).await.unwrap();
    store.delete(missing).await.unwrap();
    assert_eq!(store.has(&[chunks[0].0]).await.unwrap(), [false]);
}

#[tokio::test]
async fn object_store_round_trip_in_memory() {
    exercise(&ObjectBlobStore::new(Arc::new(InMemory::new()))).await;
}

#[tokio::test]
async fn object_store_round_trip_on_s3() {
    let Some(config) = kelp_testkit::s3::config() else {
        eprintln!("KELP_TEST_S3_ENDPOINT is not set; skipping");
        return;
    };
    exercise(&ObjectBlobStore::s3(&config).unwrap()).await;
}

#[tokio::test]
async fn cold_chunks_move_out_and_come_back_on_read() {
    let sandbox = Sandbox::new();
    let cold = ObjectBlobStore::new(Arc::new(InMemory::new()));
    let store = TieredStore::new(PackStore::open(sandbox.dir("hot")).unwrap(), cold.clone());
    let (current, history) = (chunk(1, 5000), chunk(2, 5000));
    for (id, data) in [&current, &history] {
        store.put(*id, data.clone()).await.unwrap();
    }
    store.sync().await.unwrap();

    let moved = store.demote(&HashSet::from([current.0])).await.unwrap();
    assert_eq!((moved.chunks, moved.bytes), (1, 5000));
    assert_eq!(
        cold.has(&[current.0, history.0]).await.unwrap(),
        [false, true]
    );
    assert_eq!(
        store.has(&[current.0, history.0]).await.unwrap(),
        [true, true]
    );

    assert_eq!(
        store.get(history.0).await.unwrap(),
        history.1,
        "reads fall through to the cold tier"
    );
    let again = store
        .demote(&HashSet::from([current.0, history.0]))
        .await
        .unwrap();
    assert_eq!(
        again.chunks, 0,
        "a read brought the chunk back to the hot tier"
    );
    assert_eq!(store.ids().await.unwrap().len(), 2);
}
