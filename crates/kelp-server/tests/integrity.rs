//! Regression tests for ways the server could lose data.

use std::time::Duration;

use bytes::Bytes;
use kelp_core::{ChunkId, ChunkRef, ContentId, FileManifest, LocalClId};
use kelp_proto::{Reply, Request};
use kelp_server::GcPause;
use kelp_testkit::net::{ADMIN, TestClient, TestServer};
use kelp_testkit::{depot_path, pseudo_random};

const HOUR: u64 = 3600 * 1000;

async fn fsck_clean(server: &TestServer) {
    let Reply::Fsck(report) = server.admin(Request::Fsck { deep: true }).await else {
        panic!("unexpected reply");
    };
    assert!(
        report.errors.is_empty()
            && report.missing_manifests.is_empty()
            && report.missing_chunks.is_empty()
            && report.corrupt_chunks.is_empty(),
        "{report:?}"
    );
}

async fn paused_server() -> (TestServer, GcPause) {
    let pause = GcPause::default();
    let server = TestServer::start_configured(|c| c.gc_pause = Some(pause.clone())).await;
    server.create_depot("game").await;
    server.clock().advance(2 * HOUR);
    (server, pause)
}

#[tokio::test]
async fn content_put_to_use_while_collecting_is_kept() {
    let (server, pause) = paused_server().await;
    let conn = server.add_user("alice").await;
    // An orphan chunk: uploaded, never submitted, its lease long expired.
    let data = pseudo_random(40_000, 1);
    let id = ChunkId::of(&data);
    conn.put_chunks(vec![(id, Bytes::from(data.clone()))])
        .await
        .unwrap();
    server.clock().advance(2 * HOUR);

    let admin = server.login(ADMIN).await;
    let gc = tokio::spawn(async move { admin.call(&Request::Gc { dry_run: false }).await });
    pause.marked.notified().await;

    // While the collection has already marked the chunk dead, a client
    // submits a file made of it, skipping the upload because it exists.
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/reused.bin", &data);
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("reuse".into()),
    )
    .await
    .unwrap();

    pause.resume.notify_one();
    let Reply::Gc(report) = gc.await.unwrap().unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(report.chunks_deleted, 0, "{report:?}");
    fsck_clean(&server).await;
    let other = TestClient::new();
    let mut ws2 = other.create_workspace(&server, "ws", &depot_path("//game/main"));
    ws2.sync(&conn, &other.cache(), None).await.unwrap();
    assert_eq!(other.sandbox().read("ws/reused.bin"), data);
}

#[tokio::test]
async fn obliterate_waits_for_a_collection_in_progress() {
    let (server, pause) = paused_server().await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/keep/a.txt", "a");
    client.sandbox().write("ws/secret/key.txt", "k");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("one".into()),
    )
    .await
    .unwrap();
    client.sandbox().write("ws/keep/b.txt", "b");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("two".into()),
    )
    .await
    .unwrap();

    let admin = server.login(ADMIN).await;
    let gc = tokio::spawn(async move { admin.call(&Request::Gc { dry_run: false }).await });
    pause.marked.notified().await;
    let admin = server.login(ADMIN).await;
    let obliterate = tokio::spawn(async move {
        admin
            .call(&Request::Obliterate {
                paths: vec!["//game/main/secret/...".into()],
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !obliterate.is_finished(),
        "obliterate waits for the collection"
    );
    pause.resume.notify_one();
    gc.await.unwrap().unwrap();
    obliterate.await.unwrap().unwrap();
    fsck_clean(&server).await;
    let Reply::Describe(first) = conn.call(&Request::Describe { cl: 1 }).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(first.files.len(), 1, "history without the secret is intact");
}

#[tokio::test]
async fn manifests_are_verified_and_never_replaced() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let data = pseudo_random(20_000, 3);
    let id = ChunkId::of(&data);
    conn.put_chunks(vec![(id, Bytes::from(data.clone()))])
        .await
        .unwrap();
    let manifest = |size: u64| FileManifest {
        content: ContentId(id.0),
        size,
        chunks: vec![ChunkRef {
            id,
            len: size as u32,
        }],
    };
    let wrong = conn
        .call(&Request::PutManifests {
            manifests: vec![manifest(5)],
        })
        .await;
    assert!(wrong.is_err(), "a single chunk's length is checked");
    conn.call(&Request::PutManifests {
        manifests: vec![manifest(20_000)],
    })
    .await
    .unwrap();
    conn.call(&Request::PutManifests {
        manifests: vec![manifest(5)],
    })
    .await
    .unwrap();
    let Reply::Manifests(stored) = conn
        .call(&Request::GetManifests {
            ids: vec![ContentId(id.0)],
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(
        stored[0].as_ref().unwrap().size,
        20_000,
        "the verified manifest stays"
    );
}
