use bytes::Bytes;
use kelp_cas::{BlobStore, PackStore};
use kelp_core::ChunkId;
use kelp_testkit::{Sandbox, pseudo_random};

fn chunk(seed: u64, len: usize) -> (ChunkId, Bytes) {
    let data = pseudo_random(len, seed);
    (ChunkId::of(&data), data.into())
}

#[tokio::test]
async fn small_chunks_are_packed_and_survive_reopening() {
    let sandbox = Sandbox::new();
    let store = PackStore::open(sandbox.dir("store")).unwrap();
    let small: Vec<_> = (0..500).map(|i| chunk(i, 100 + i as usize)).collect();
    let large = chunk(9_999, 1 << 20);
    for (id, data) in small.iter().chain([&large]) {
        store.put(*id, data.clone()).await.unwrap();
    }
    assert_eq!(
        store.get(small[7].0).await.unwrap(),
        small[7].1,
        "readable before sync"
    );
    store.sync().await.unwrap();

    let files = std::fs::read_dir(sandbox.path("store/packs"))
        .unwrap()
        .count();
    assert_eq!(files, 2, "one pack and its index for 500 chunks");
    drop(store);

    let store = PackStore::open(sandbox.path("store")).unwrap();
    for (id, data) in small.iter().chain([&large]) {
        assert_eq!(&store.get(*id).await.unwrap(), data);
    }
    let mut ids = store.ids().await.unwrap();
    ids.sort();
    let mut expected: Vec<_> = small.iter().chain([&large]).map(|(id, _)| *id).collect();
    expected.sort();
    assert_eq!(ids, expected);
    let missing = ChunkId::of(b"missing");
    assert_eq!(
        store.has(&[small[0].0, large.0, missing]).await.unwrap(),
        [true, true, false]
    );
}

#[tokio::test]
async fn deleting_most_of_a_pack_lets_compaction_reclaim_it() {
    let sandbox = Sandbox::new();
    let store = PackStore::open(sandbox.dir("store")).unwrap();
    let chunks: Vec<_> = (0..100).map(|i| chunk(i, 2_000)).collect();
    for (id, data) in &chunks {
        store.put(*id, data.clone()).await.unwrap();
    }
    store.sync().await.unwrap();

    // Start a new pack so the full one is no longer active.
    drop(store);
    let reopened = PackStore::open(sandbox.path("store")).unwrap();
    for (id, _) in &chunks[..70] {
        reopened.delete(*id).await.unwrap();
    }
    reopened
        .put(chunk(500, 10).0, chunk(500, 10).1)
        .await
        .unwrap();
    reopened.sync().await.unwrap();

    let result = reopened.compact().await.unwrap();
    assert_eq!(result.packs, 1);
    assert!(result.freed > 100 * 2_000 / 2);
    for (id, data) in &chunks[70..] {
        assert_eq!(
            &reopened.get(*id).await.unwrap(),
            data,
            "live chunks move to a new pack"
        );
    }
    assert!(reopened.get(chunks[0].0).await.is_err());
    assert_eq!(reopened.compact().await.unwrap().packs, 0);
}

#[tokio::test]
async fn compaction_keeps_chunks_written_after_a_torn_record() {
    let sandbox = Sandbox::new();
    let before: Vec<_> = (0..20).map(|i| chunk(i, 2_000)).collect();
    {
        let store = PackStore::open(sandbox.dir("store")).unwrap();
        for (id, data) in &before {
            store.put(*id, data.clone()).await.unwrap();
        }
        store.sync().await.unwrap();
    }
    // A crash in the middle of a write leaves half a record at the end.
    let pack = sandbox.path("store/packs/pack-00000001.dat");
    let mut torn = std::fs::read(&pack).unwrap();
    torn.extend_from_slice(&[0xab; 50]);
    std::fs::write(&pack, &torn).unwrap();

    let store = PackStore::open(sandbox.path("store")).unwrap();
    let after: Vec<_> = (100..120).map(|i| chunk(i, 2_000)).collect();
    for (id, data) in &after {
        store.put(*id, data.clone()).await.unwrap();
    }
    store.sync().await.unwrap();
    for (id, _) in &before {
        store.delete(*id).await.unwrap();
    }
    store.sync().await.unwrap();
    let compaction = store.compact().await.unwrap();
    assert_eq!(
        compaction.packs, 1,
        "the torn pack held only deleted chunks"
    );
    for (id, data) in &after {
        assert_eq!(&store.get(*id).await.unwrap(), data);
    }
}

#[tokio::test]
async fn an_empty_loose_chunk_left_by_a_crash_is_replaced() {
    let sandbox = Sandbox::new();
    let store = kelp_cas::FsBlobStore::new(sandbox.dir("loose"));
    let (id, data) = chunk(5, 10_000);
    store.put(id, data.clone()).await.unwrap();
    let hex = id.to_string();
    let path = sandbox.path(&format!("loose/{}/{}/{hex}", &hex[..2], &hex[2..4]));
    std::fs::write(&path, b"").unwrap();
    assert!(store.get(id).await.is_err());
    store.put(id, data.clone()).await.unwrap();
    assert_eq!(store.get(id).await.unwrap(), data);
}
