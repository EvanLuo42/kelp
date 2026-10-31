//! Servers storing chunks in S3; skipped unless KELP_TEST_S3_ENDPOINT is set.

use kelp_core::LocalClId;
use kelp_proto::{Reply, Request};
use kelp_server::{StorageConfig, StorageKind};
use kelp_testkit::net::{TestClient, TestServer};
use kelp_testkit::{depot_path, pseudo_random};
use kelp_workspace::OpenAction;

async fn server(kind: StorageKind) -> Option<TestServer> {
    let Some(s3) = kelp_testkit::s3::config() else {
        eprintln!("KELP_TEST_S3_ENDPOINT is not set; skipping");
        return None;
    };
    Some(TestServer::start_with(Some(StorageConfig { kind, s3: Some(s3) })).await)
}

#[tokio::test]
async fn s3_storage_round_trip() {
    let Some(server) = server(StorageKind::S3).await else {
        return;
    };
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let (a, b) = (TestClient::new(), TestClient::new());
    let stream = depot_path("//game/main");
    let mut wa = a.create_workspace(&server, "ws", &stream);
    a.sandbox().write("ws/big.bin", pseudo_random(2 << 20, 1));
    wa.reconcile(&conn).await.unwrap();
    wa.submit(&conn, &a.cache(), LocalClId::DEFAULT, Some("big".into()))
        .await
        .unwrap();
    let mut wb = b.create_workspace(&server, "ws", &stream);
    wb.sync(&conn, &b.cache(), None).await.unwrap();
    assert_eq!(b.sandbox().read("ws/big.bin"), pseudo_random(2 << 20, 1));
}

#[tokio::test]
async fn tiering_moves_history_to_the_bucket() {
    let Some(server) = server(StorageKind::Tiered).await else {
        return;
    };
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let a = TestClient::new();
    let stream = depot_path("//game/main");
    let mut wa = a.create_workspace(&server, "ws", &stream);
    a.sandbox().write("ws/level.bin", pseudo_random(300_000, 1));
    wa.reconcile(&conn).await.unwrap();
    wa.submit(&conn, &a.cache(), LocalClId::DEFAULT, Some("v1".into()))
        .await
        .unwrap();
    let level = depot_path("//game/main/level.bin");
    wa.open_files(
        &conn,
        std::slice::from_ref(&level),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    a.sandbox().write("ws/level.bin", pseudo_random(300_000, 2));
    wa.submit(&conn, &a.cache(), LocalClId::DEFAULT, Some("v2".into()))
        .await
        .unwrap();

    let Reply::Tiered { chunks, bytes } = server.admin(Request::Tier).await else {
        panic!()
    };
    assert!(
        chunks > 0 && bytes >= 300_000,
        "the first version's chunks move out ({chunks}, {bytes})"
    );

    // A fresh machine can still sync the old version, read from the bucket.
    let b = TestClient::new();
    let mut wb = b.create_workspace(&server, "ws", &stream);
    wb.sync(&conn, &b.cache(), Some(1)).await.unwrap();
    assert_eq!(b.sandbox().read("ws/level.bin"), pseudo_random(300_000, 1));
}

#[tokio::test]
async fn tiering_needs_tiered_storage() {
    let server = TestServer::start().await;
    let admin = server.login(kelp_testkit::net::ADMIN).await;
    assert!(admin.call(&Request::Tier).await.is_err());
}
