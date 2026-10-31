use kelp_client::{Error, SkipReason};
use kelp_core::LocalClId;
use kelp_proto::rpc::FileAction;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

fn stream() -> kelp_core::DepotPath {
    depot_path("//game/main")
}

#[tokio::test]
async fn submit_then_sync_on_another_machine() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    let (alice, bob) = (TestClient::new(), TestClient::new());
    let mut ws_a = alice.create_workspace(&server, "ws", &stream());
    let mut ws_b = bob.create_workspace(&server, "ws", &stream());
    let big = kelp_testkit::pseudo_random(3 << 20, 7);
    alice.sandbox().write("ws/Source/main.rs", "fn main() {}\n");
    alice.sandbox().write("ws/Content/Hero.png", &big);
    alice.sandbox().write("ws/Saved/log.txt", "ignored");
    alice.sandbox().write("ws/.kelpignore", "/Saved/\n");
    let opened = ws_a.reconcile(&alice_conn).await.unwrap();
    assert_eq!(opened.len(), 3, "{opened:?}");
    let cl1 = ws_a
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("initial import".into()),
        )
        .await
        .unwrap();
    assert_eq!(cl1, 1);
    assert!(!ws_a.status().unwrap().has_unopened_changes());

    let report = ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert_eq!((report.cl, report.updated.len()), (1, 3));
    assert_eq!(bob.sandbox().read("ws/Content/Hero.png"), big);
    assert_eq!(bob.sandbox().read("ws/Source/main.rs"), b"fn main() {}\n");
    assert!(!bob.sandbox().exists("ws/Saved/log.txt"));

    // Bob edits in a numbered changelist and deletes a file.
    let fix = ws_b.pending_mut().create("fix main");
    let main_rs = depot_path("//game/main/Source/main.rs");
    ws_b.open_files(
        &bob_conn,
        std::slice::from_ref(&main_rs),
        OpenAction::Edit,
        fix,
    )
    .await
    .unwrap();
    bob.sandbox()
        .write("ws/Source/main.rs", "fn main() { println!(\"hi\"); }\n");
    ws_b.open_files(
        &bob_conn,
        &[depot_path("//game/main/.kelpignore")],
        OpenAction::Delete,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    assert!(!bob.sandbox().exists("ws/.kelpignore"));
    let cl2 = ws_b
        .submit(&bob_conn, &bob.cache(), fix, None)
        .await
        .unwrap();
    let cl3 = ws_b
        .submit(
            &bob_conn,
            &bob.cache(),
            LocalClId::DEFAULT,
            Some("drop ignore file".into()),
        )
        .await
        .unwrap();
    assert_eq!((cl2, cl3), (2, 3));

    let report = ws_a.sync(&alice_conn, &alice.cache(), None).await.unwrap();
    assert_eq!(report.cl, 3);
    assert_eq!(
        alice.sandbox().read("ws/Source/main.rs"),
        b"fn main() { println!(\"hi\"); }\n"
    );
    assert!(!alice.sandbox().exists("ws/.kelpignore"));

    // Syncing back to the first changelist restores the old files.
    let report = ws_a
        .sync(&alice_conn, &alice.cache(), Some(1))
        .await
        .unwrap();
    assert_eq!(report.cl, 1);
    assert_eq!(alice.sandbox().read("ws/Source/main.rs"), b"fn main() {}\n");
    assert!(alice.sandbox().exists("ws/.kelpignore"));

    let Reply::Filelog(revs) = alice_conn
        .call(&Request::Filelog {
            path: main_rs.to_string(),
        })
        .await
        .unwrap()
    else {
        panic!("expected filelog");
    };
    assert_eq!(
        revs.iter()
            .map(|r| (r.rev, r.cl, r.action))
            .collect::<Vec<_>>(),
        vec![(2, 2, FileAction::Edit), (1, 1, FileAction::Add)]
    );
    let Reply::Describe(detail) = alice_conn.call(&Request::Describe { cl: 2 }).await.unwrap()
    else {
        panic!("expected describe");
    };
    assert_eq!(
        (
            detail.summary.author.as_str(),
            detail.summary.description.as_str()
        ),
        ("bob", "fix main")
    );
}

#[tokio::test]
async fn concurrent_edits_conflict_and_sync_protects_local_work() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    let (alice, bob) = (TestClient::new(), TestClient::new());
    let mut ws_a = alice.create_workspace(&server, "ws", &stream());
    let mut ws_b = bob.create_workspace(&server, "ws", &stream());
    let path = depot_path("//game/main/notes.txt");

    alice.sandbox().write("ws/notes.txt", "v1\n");
    ws_a.open_files(
        &alice_conn,
        std::slice::from_ref(&path),
        OpenAction::Add,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    ws_a.submit(
        &alice_conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("v1".into()),
    )
    .await
    .unwrap();
    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();

    for (ws, client, conn, text) in [
        (&mut ws_a, &alice, &alice_conn, "alice\n"),
        (&mut ws_b, &bob, &bob_conn, "bob\n"),
    ] {
        ws.open_files(
            conn,
            std::slice::from_ref(&path),
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap();
        client.sandbox().write("ws/notes.txt", text);
    }
    ws_a.submit(
        &alice_conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("alice".into()),
    )
    .await
    .unwrap();
    let err = ws_b
        .submit(
            &bob_conn,
            &bob.cache(),
            LocalClId::DEFAULT,
            Some("bob".into()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Conflict),
        "{err}"
    );
    assert_eq!(
        ws_b.state().pending.action(&path),
        Some(OpenAction::Edit),
        "failed submit keeps the file opened"
    );

    // Sync never overwrites opened files or unopened local edits.
    let report = ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert_eq!(report.skipped, vec![(path.clone(), SkipReason::Opened)]);
    assert_eq!(bob.sandbox().read("ws/notes.txt"), b"bob\n");

    ws_b.revert(&bob_conn, &bob.cache(), std::slice::from_ref(&path))
        .await
        .unwrap();
    assert_eq!(bob.sandbox().read("ws/notes.txt"), b"v1\n");
    bob.sandbox()
        .write("ws/notes.txt", "edited without opening\n");
    let report = ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert_eq!(
        report.skipped,
        vec![(path.clone(), SkipReason::LocallyModified)]
    );
    assert_eq!(ws_b.status().unwrap().modified, vec![path.clone()]);

    bob.sandbox().write("ws/notes.txt", "v1\n");
    let report = ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert_eq!(report.updated, vec![path]);
    assert_eq!(bob.sandbox().read("ws/notes.txt"), b"alice\n");
}

#[tokio::test]
async fn unchanged_chunks_are_not_uploaded_twice() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &stream());

    let mut data = kelp_testkit::pseudo_random(4 << 20, 1);
    client.sandbox().write("ws/level.bin", &data);
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("level".into()),
    )
    .await
    .unwrap();

    data.splice(1_000_000..1_000_000, *b"a small insertion");
    client.sandbox().write("ws/level.bin", &data);
    ws.reconcile(&conn).await.unwrap();
    let manifest = client.cache().ingest(&data).await.unwrap();
    let ids: Vec<_> = manifest.chunks.iter().map(|c| c.id).collect();
    let Reply::Has(has) = conn.call(&Request::HasChunks { ids }).await.unwrap() else {
        panic!("expected has")
    };
    let missing = has.iter().filter(|h| !**h).count();
    assert!(
        missing <= 3,
        "{missing} of {} chunks would be uploaded",
        has.len()
    );
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("tweak".into()),
    )
    .await
    .unwrap();
}

async fn submit_all(
    ws: &mut kelp_client::Workspace,
    conn: &kelp_client::Connection,
    client: &kelp_testkit::net::TestClient,
) {
    ws.reconcile(conn).await.unwrap();
    ws.submit(
        conn,
        &client.cache(),
        kelp_core::LocalClId::DEFAULT,
        Some("c".into()),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn sync_turns_a_file_into_a_directory_and_back() {
    let server = kelp_testkit::net::TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let (a, b) = (
        kelp_testkit::net::TestClient::new(),
        kelp_testkit::net::TestClient::new(),
    );
    let stream = kelp_testkit::depot_path("//game/main");
    let mut ws_a = a.create_workspace(&server, "ws", &stream);
    let mut ws_b = b.create_workspace(&server, "wsb", &stream);
    a.sandbox().write("ws/x", "file");
    submit_all(&mut ws_a, &conn, &a).await;
    ws_b.sync(&conn, &b.cache(), None).await.unwrap();

    std::fs::remove_file(a.sandbox().path("ws/x")).unwrap();
    a.sandbox().write("ws/x/y", "inside");
    submit_all(&mut ws_a, &conn, &a).await;
    ws_b.sync(&conn, &b.cache(), None).await.unwrap();
    assert_eq!(b.sandbox().read("wsb/x/y"), b"inside");

    std::fs::remove_dir_all(a.sandbox().path("ws/x")).unwrap();
    a.sandbox().write("ws/x", "file again");
    submit_all(&mut ws_a, &conn, &a).await;
    ws_b.sync(&conn, &b.cache(), None).await.unwrap();
    assert_eq!(b.sandbox().read("wsb/x"), b"file again");
}
