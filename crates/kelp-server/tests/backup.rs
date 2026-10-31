use kelp_cas::{BlobStore, PackStore};
use kelp_core::{ChunkId, LocalClId};
use kelp_proto::{Reply, Request};
use kelp_server::Error;
use kelp_server::backup::{open_offline, restore};
use kelp_testkit::net::{TestClient, TestServer};
use kelp_testkit::{Sandbox, depot_path, pseudo_random};

async fn backup(server: &TestServer, dest: &std::path::Path, metadata_only: bool) -> (u64, u64) {
    match server
        .admin(Request::Backup {
            dest: dest.display().to_string(),
            metadata_only,
        })
        .await
    {
        Reply::BackedUp {
            chunks_copied,
            chunks_present,
            ..
        } => (chunks_copied, chunks_present),
        other => panic!("unexpected reply {other:?}"),
    }
}

#[tokio::test]
async fn a_backup_restores_onto_another_machine_with_the_same_identity() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let alice = TestClient::new();
    let stream = depot_path("//game/main");
    let mut ws = alice.create_workspace(&server, "ws", &stream);
    alice
        .sandbox()
        .write("ws/Content/hero.bin", pseudo_random(3 << 20, 1));
    alice.sandbox().write("ws/readme.txt", "hello\n");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("first".into()),
    )
    .await
    .unwrap();

    let elsewhere = Sandbox::new();
    let dest = elsewhere.path("backup");
    let (copied, present) = backup(&server, &dest, false).await;
    assert!(copied > 1 && present == 0, "{copied} {present}");

    alice.sandbox().write("ws/notes.txt", "more\n");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("second".into()),
    )
    .await
    .unwrap();
    let (copied_again, present_again) = backup(&server, &dest, false).await;
    assert_eq!(
        (copied_again, present_again),
        (1, copied),
        "only the new file is copied"
    );

    let data = elsewhere.path("restored");
    let report = restore(&dest, &data, false).await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert!(report.audit_entries > 0);

    let moved = server.start_on(&data);
    assert_eq!(
        moved.fingerprint(),
        server.fingerprint(),
        "clients keep trusting it"
    );
    let bob = TestClient::new();
    let bob_conn = moved.login("alice").await;
    let mut ws_b = bob.create_workspace(&moved, "ws", &stream);
    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert_eq!(
        bob.sandbox().read("ws/Content/hero.bin"),
        pseudo_random(3 << 20, 1)
    );
    assert_eq!(bob.sandbox().read("ws/notes.txt"), b"more\n");
    let Reply::AuditVerified { .. } = moved.admin(Request::VerifyAudit).await else {
        panic!("unexpected reply");
    };
}

#[tokio::test]
async fn restore_checks_its_input_and_reports_damage() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let alice = TestClient::new();
    let mut ws = alice.create_workspace(&server, "ws", &depot_path("//game/main"));
    alice.sandbox().write("ws/a.bin", pseudo_random(50_000, 2));
    ws.reconcile(&conn).await.unwrap();
    ws.submit(&conn, &alice.cache(), LocalClId::DEFAULT, Some("a".into()))
        .await
        .unwrap();

    assert!(
        matches!(open_offline(&server.data_dir()), Err(Error::Config(ref m)) if m.contains("server is using")),
        "a running server's directory is not opened twice"
    );

    let place = Sandbox::new();
    let dest = place.path("backup");
    backup(&server, &dest, false).await;
    assert!(matches!(
        restore(&place.dir("not-a-backup"), &place.path("x"), false).await,
        Err(Error::Config(_))
    ));
    place.write("busy/file", "x");
    assert!(matches!(
        restore(&dest, &place.path("busy"), false).await,
        Err(Error::AlreadyInitialised(_))
    ));

    let chunk = ChunkId::of(&pseudo_random(50_000, 2));
    {
        let store = PackStore::open(dest.join("chunks")).unwrap();
        store.delete(chunk).await.unwrap();
        store.sync().await.unwrap();
    }
    let report = restore(&dest, &place.path("damaged"), true).await.unwrap();
    assert_eq!(report.missing_chunks, [chunk]);
    assert!(!place.exists("damaged/tls"), "a new identity is generated");

    let meta_only = place.path("meta-only");
    let (copied, _) = backup(&server, &meta_only, true).await;
    assert_eq!(copied, 0);
    assert!(!place.exists("meta-only/chunks"));
}

#[tokio::test]
async fn clients_follow_a_moved_server() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let client = TestClient::new();
    client.login(&server, kelp_testkit::net::ADMIN).await;
    let stream = depot_path("//game/main");
    let mut ws = client.create_workspace(&server, "ws", &stream);
    let conn = client.home().connect(server.addr()).await.unwrap();
    client.sandbox().write("ws/a.txt", "a");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(&conn, &client.cache(), LocalClId::DEFAULT, Some("a".into()))
        .await
        .unwrap();

    let place = Sandbox::new();
    backup(&server, &place.path("backup"), false).await;
    restore(&place.path("backup"), &place.path("data"), false)
        .await
        .unwrap();
    let moved = server.start_on(&place.path("data"));

    client
        .home()
        .move_login(server.addr(), moved.addr())
        .unwrap();
    ws.set_server(moved.addr()).unwrap();
    let reopened = kelp_client::Workspace::open(&client.workspace_root("ws")).unwrap();
    assert_eq!(reopened.config().server, moved.addr());
    let conn = client.home().connect(moved.addr()).await.unwrap();
    let Reply::Changes(changes) = conn
        .call(&Request::Changes {
            stream: None,
            limit: 10,
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(
        changes.len(),
        1,
        "the moved server has the history and accepts the old token"
    );
}
