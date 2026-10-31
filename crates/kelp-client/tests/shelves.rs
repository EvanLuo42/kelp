use kelp_client::Error;
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

#[tokio::test]
async fn shelved_changes_move_between_workspaces() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let stream = depot_path("//game/main");
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    let (alice, bob) = (TestClient::new(), TestClient::new());
    let mut ws_a = alice.create_workspace(&server, "ws", &stream);
    let mut ws_b = bob.create_workspace(&server, "ws", &stream);

    alice.sandbox().write("ws/a.txt", "base\n");
    ws_a.reconcile(&alice_conn).await.unwrap();
    ws_a.submit(
        &alice_conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("base".into()),
    )
    .await
    .unwrap();
    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();

    let review = ws_a.pending_mut().create("needs review");
    let a_txt = depot_path("//game/main/a.txt");
    ws_a.open_files(
        &alice_conn,
        std::slice::from_ref(&a_txt),
        OpenAction::Edit,
        review,
    )
    .await
    .unwrap();
    alice.sandbox().write("ws/a.txt", "draft 1\n");
    alice.sandbox().write("ws/new.txt", "new\n");
    ws_a.open_files(
        &alice_conn,
        &[depot_path("//game/main/new.txt")],
        OpenAction::Add,
        review,
    )
    .await
    .unwrap();
    let shelf = ws_a
        .shelve(&alice_conn, &alice.cache(), review)
        .await
        .unwrap();
    alice.sandbox().write("ws/a.txt", "draft 2\n");
    assert_eq!(
        ws_a.shelve(&alice_conn, &alice.cache(), review)
            .await
            .unwrap(),
        shelf,
        "reshelving keeps the number"
    );

    let (cl, opened) = ws_b
        .unshelve(&bob_conn, &bob.cache(), shelf, None)
        .await
        .unwrap();
    assert_eq!(opened.len(), 2);
    assert_eq!(bob.sandbox().read("ws/a.txt"), b"draft 2\n");
    assert_eq!(bob.sandbox().read("ws/new.txt"), b"new\n");

    let err = bob_conn
        .call(&Request::DeleteShelf { cl: shelf })
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
    ws_b.submit(
        &bob_conn,
        &bob.cache(),
        cl,
        Some("landed alice's change".into()),
    )
    .await
    .unwrap();
    alice_conn
        .call(&Request::DeleteShelf { cl: shelf })
        .await
        .unwrap();
}
