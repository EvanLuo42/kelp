use kelp_client::{Connection, Workspace};
use kelp_core::LocalClId;
use kelp_proto::Request;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

async fn submit_cl(
    ws: &mut Workspace,
    conn: &Connection,
    client: &TestClient,
    cl: LocalClId,
) -> u64 {
    ws.submit(conn, &client.cache(), cl, Some("c".into()))
        .await
        .unwrap()
}

async fn edit(
    ws: &mut Workspace,
    conn: &Connection,
    client: &TestClient,
    file: &str,
    text: &str,
) -> u64 {
    let path = depot_path(&format!("{}/{file}", ws.stream()));
    ws.open_files(conn, &[path], OpenAction::Edit, LocalClId::DEFAULT)
        .await
        .unwrap();
    client.sandbox().write(&format!("ws/{file}"), text);
    submit_cl(ws, conn, client, LocalClId::DEFAULT).await
}

#[tokio::test]
async fn undo_reverses_a_changelist() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "good\n");
    client.sandbox().write("ws/b.txt", "b\n");
    ws.reconcile(&conn).await.unwrap();
    submit_cl(&mut ws, &conn, &client, LocalClId::DEFAULT).await;
    let bad = edit(&mut ws, &conn, &client, "a.txt", "bad\n").await;

    ws.sync(&conn, &client.cache(), None).await.unwrap();
    let report = ws.undo(&conn, &client.cache(), bad).await.unwrap();
    assert!(report.conflicts.is_empty());
    assert_eq!(client.sandbox().read("ws/a.txt"), b"good\n");
    submit_cl(&mut ws, &conn, &client, report.changelist.unwrap()).await;

    let later = edit(&mut ws, &conn, &client, "b.txt", "b2\n").await;
    edit(&mut ws, &conn, &client, "b.txt", "b3\n").await;
    ws.sync(&conn, &client.cache(), None).await.unwrap();
    let report = ws.undo(&conn, &client.cache(), later).await.unwrap();
    assert_eq!(
        report.conflicts,
        [depot_path("//game/main/b.txt")],
        "changed since"
    );
}

#[tokio::test]
async fn one_changelist_can_be_picked_from_another_stream() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let (main, dev) = (TestClient::new(), TestClient::new());
    let mut ws_main = main.create_workspace(&server, "ws", &depot_path("//game/main"));
    main.sandbox().write("ws/a.txt", "a\n");
    main.sandbox().write("ws/b.txt", "b\n");
    ws_main.reconcile(&conn).await.unwrap();
    submit_cl(&mut ws_main, &conn, &main, LocalClId::DEFAULT).await;
    server
        .admin(Request::CreateStream {
            path: "//game/dev".into(),
            kind: kelp_proto::rpc::StreamKind::Development,
            parent: Some("//game/main".into()),
        })
        .await;
    let mut ws_dev = dev.create_workspace(&server, "ws", &depot_path("//game/dev"));
    ws_dev.sync(&conn, &dev.cache(), None).await.unwrap();
    let fix = edit(&mut ws_dev, &conn, &dev, "a.txt", "fixed\n").await;
    edit(&mut ws_dev, &conn, &dev, "b.txt", "experimental\n").await;

    ws_main.sync(&conn, &main.cache(), None).await.unwrap();
    let report = ws_main
        .cherry_pick(&conn, &main.cache(), None, fix)
        .await
        .unwrap();
    assert_eq!(
        report.opened,
        [(depot_path("//game/main/a.txt"), OpenAction::Edit)]
    );
    assert_eq!(main.sandbox().read("ws/a.txt"), b"fixed\n");
    assert_eq!(
        main.sandbox().read("ws/b.txt"),
        b"b\n",
        "only the picked change"
    );
    let cl = report.changelist.unwrap();
    assert!(
        ws_main
            .state()
            .pending
            .changelist(cl)
            .unwrap()
            .integration
            .is_none()
    );
}
