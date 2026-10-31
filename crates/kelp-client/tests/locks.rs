use kelp_client::{Error, is_read_only};
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

const TYPEMAP: &str = "[[rule]]\npattern = \"**/*.png\"\nkind = \"binary\"\nlock = \"exclusive\"\n";

#[tokio::test]
async fn exclusive_files_are_read_only_until_locked() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server
        .admin(Request::SetTypemap {
            depot: "game".into(),
            source: TYPEMAP.into(),
        })
        .await;
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    let (alice, bob) = (TestClient::new(), TestClient::new());
    let stream = depot_path("//game/main");
    let mut ws_a = alice.create_workspace(&server, "ws", &stream);
    let mut ws_b = bob.create_workspace(&server, "ws", &stream);
    let hero = depot_path("//game/main/Hero.png");

    alice.sandbox().write("ws/Hero.png", [1, 2, 3]);
    ws_a.reconcile(&alice_conn).await.unwrap();
    ws_a.submit(
        &alice_conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("hero".into()),
    )
    .await
    .unwrap();
    assert!(
        is_read_only(&alice.sandbox().path("ws/Hero.png")).unwrap(),
        "read-only again after submit"
    );

    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert!(is_read_only(&bob.sandbox().path("ws/Hero.png")).unwrap());

    ws_a.open_files(
        &alice_conn,
        std::slice::from_ref(&hero),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    assert!(!is_read_only(&alice.sandbox().path("ws/Hero.png")).unwrap());
    let err = ws_b
        .open_files(
            &bob_conn,
            std::slice::from_ref(&hero),
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Conflict),
        "{err}"
    );
    assert!(ws_b.state().pending.action(&hero).is_none());

    ws_a.revert(&alice_conn, &alice.cache(), std::slice::from_ref(&hero))
        .await
        .unwrap();
    assert!(is_read_only(&alice.sandbox().path("ws/Hero.png")).unwrap());
    ws_b.open_files(
        &bob_conn,
        std::slice::from_ref(&hero),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    bob.sandbox().write("ws/Hero.png", [4, 5, 6]);
    ws_b.submit(
        &bob_conn,
        &bob.cache(),
        LocalClId::DEFAULT,
        Some("bob's hero".into()),
    )
    .await
    .unwrap();

    // Submitting releases the lock, so alice can take it after syncing.
    ws_a.sync(&alice_conn, &alice.cache(), None).await.unwrap();
    ws_a.open_files(&alice_conn, &[hero], OpenAction::Edit, LocalClId::DEFAULT)
        .await
        .unwrap();
}

#[tokio::test]
async fn submitting_an_exclusive_file_requires_its_lock() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/Hero.png", [1]);
    ws.reconcile(&conn).await.unwrap();

    // The typemap changes after the file was opened, so no lock was taken.
    server
        .admin(Request::SetTypemap {
            depot: "game".into(),
            source: TYPEMAP.into(),
        })
        .await;
    let err = ws
        .submit(&conn, &client.cache(), LocalClId::DEFAULT, Some("x".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Conflict),
        "{err}"
    );
}
