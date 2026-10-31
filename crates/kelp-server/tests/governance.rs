use bytes::Bytes;
use kelp_client::Error;
use kelp_core::{ChunkId, LocalClId};
use kelp_proto::rpc::GcReport;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::net::{ADMIN, TestClient, TestServer};
use kelp_testkit::{depot_path, pseudo_random};

const HOUR: u64 = 3600 * 1000;

async fn gc(server: &TestServer, dry_run: bool) -> GcReport {
    match server.admin(Request::Gc { dry_run }).await {
        Reply::Gc(report) => report,
        other => panic!("unexpected reply {other:?}"),
    }
}

#[tokio::test]
async fn gc_waits_for_leases_after_start_up() {
    let server = TestServer::start().await;
    let admin = server.login(ADMIN).await;
    let err = admin
        .call(&Request::Gc { dry_run: true })
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Conflict),
        "{err}"
    );
}

#[tokio::test]
async fn uploads_are_kept_until_their_lease_ends() {
    let server = TestServer::start().await;
    server.clock().advance(2 * HOUR);
    let conn = server.add_user("alice").await;
    let data = Bytes::from(pseudo_random(1000, 1));
    let id = ChunkId::of(&data);
    conn.put_chunks(vec![(id, data)]).await.unwrap();

    let report = gc(&server, false).await;
    assert_eq!(
        report.chunks_deleted, 0,
        "a chunk uploaded for a pending submit survives"
    );
    server.clock().advance(2 * HOUR);
    let report = gc(&server, false).await;
    assert_eq!(report.chunks_deleted, 1);
    let Reply::Has(has) = conn
        .call(&Request::HasChunks { ids: vec![id] })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(has, [false]);
}

#[tokio::test]
async fn obliterated_and_purged_content_is_collected() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let typemap = "[[rule]]\npattern = \"Baked/**\"\nretain = 1\n";
    server
        .admin(Request::SetTypemap {
            depot: "game".into(),
            source: typemap.into(),
        })
        .await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let stream = depot_path("//game/main");
    let mut ws = client.create_workspace(&server, "ws", &stream);

    client
        .sandbox()
        .write("ws/Secret/key.pem", pseudo_random(500_000, 7));
    client
        .sandbox()
        .write("ws/Baked/light.bin", pseudo_random(300_000, 8));
    client.sandbox().write("ws/readme.txt", "hello\n");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("base".into()),
    )
    .await
    .unwrap();
    let baked = depot_path("//game/main/Baked/light.bin");
    ws.open_files(
        &conn,
        std::slice::from_ref(&baked),
        kelp_workspace::OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    client
        .sandbox()
        .write("ws/Baked/light.bin", pseudo_random(300_000, 9));
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("rebake".into()),
    )
    .await
    .unwrap();

    let Reply::Filelog(revs) = conn
        .call(&Request::Filelog {
            path: baked.to_string(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        revs.iter().map(|r| r.purged).collect::<Vec<_>>(),
        [false, true]
    );

    let forbidden = conn
        .call(&Request::Obliterate {
            paths: vec!["//game/main/Secret/...".into()],
        })
        .await;
    assert!(matches!(forbidden, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden));
    server
        .admin(Request::Obliterate {
            paths: vec!["//game/main/Secret/...".into()],
        })
        .await;

    server.clock().advance(2 * HOUR);
    let preview = gc(&server, true).await;
    assert!(preview.dry_run && preview.chunks_deleted > 0);
    let report = gc(&server, false).await;
    assert_eq!(report.chunks_deleted, preview.chunks_deleted);
    assert!(
        report.nodes_deleted > 0,
        "the trees from before the obliterate are gone"
    );
    assert_eq!(gc(&server, false).await.chunks_deleted, 0);

    // A fresh workspace still syncs everything that remains.
    let other = TestClient::new();
    let mut ws2 = other.create_workspace(&server, "ws", &stream);
    ws2.sync(&conn, &other.cache(), None).await.unwrap();
    assert!(!other.sandbox().exists("ws/Secret/key.pem"));
    assert_eq!(
        other.sandbox().read("ws/Baked/light.bin"),
        pseudo_random(300_000, 9)
    );
    assert_eq!(other.sandbox().read("ws/readme.txt"), b"hello\n");

    let Reply::Audit(entries) = server
        .admin(Request::AuditLog {
            from: 0,
            limit: 100,
        })
        .await
    else {
        panic!()
    };
    let obliterate = entries
        .iter()
        .find(|e| e.summary.starts_with("obliterate"))
        .unwrap();
    assert_eq!(obliterate.actor, ADMIN);
    assert!(
        entries
            .iter()
            .any(|e| e.actor == "alice" && e.summary.starts_with("submit"))
    );
    let Reply::AuditVerified { entries: count, .. } = server.admin(Request::VerifyAudit).await
    else {
        panic!()
    };
    // Logging in for the second request issues a token, one more entry.
    assert_eq!(count as usize, entries.len() + 1);
}
