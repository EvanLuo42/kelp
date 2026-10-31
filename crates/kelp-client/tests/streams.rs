use kelp_client::Error;
use kelp_core::LocalClId;
use kelp_proto::rpc::StreamKind;
use kelp_proto::{ErrorKind, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

async fn open(
    conn: &kelp_client::Connection,
    ws: &mut kelp_client::Workspace,
    paths: Vec<kelp_core::DepotPath>,
    action: OpenAction,
) {
    ws.open_files(conn, &paths, action, LocalClId::DEFAULT)
        .await
        .unwrap();
}

#[tokio::test]
async fn merge_down_then_copy_up() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let (main, dev) = (depot_path("//game/main"), depot_path("//game/dev"));
    let mut ws_main = client.create_workspace(&server, "main", &main);
    for (path, text) in [
        ("main/a.txt", "a\n"),
        ("main/b.txt", "1\n2\n3\n4\n5\n6\n7\n8\n"),
        ("main/c.txt", "c\n"),
    ] {
        client.sandbox().write(path, text);
    }
    ws_main.reconcile(&conn).await.unwrap();
    ws_main
        .submit(
            &conn,
            &client.cache(),
            LocalClId::DEFAULT,
            Some("base".into()),
        )
        .await
        .unwrap();

    let create = Request::CreateStream {
        path: dev.to_string(),
        kind: StreamKind::Development,
        parent: Some(main.to_string()),
    };
    let err = conn.call(&create).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
    server.admin(create).await;

    let mut ws_dev = client.create_workspace(&server, "dev", &dev);
    ws_dev.sync(&conn, &client.cache(), None).await.unwrap();
    assert_eq!(
        client.sandbox().read("dev/a.txt"),
        b"a\n",
        "a new stream starts with its parent's files"
    );

    // Diverge: dev edits the start of b.txt; main edits a.txt, the end of b.txt and deletes c.txt.
    open(
        &conn,
        &mut ws_dev,
        vec![depot_path("//game/dev/b.txt")],
        OpenAction::Edit,
    )
    .await;
    client
        .sandbox()
        .write("dev/b.txt", "ONE\n2\n3\n4\n5\n6\n7\n8\n");
    ws_dev
        .submit(
            &conn,
            &client.cache(),
            LocalClId::DEFAULT,
            Some("dev work".into()),
        )
        .await
        .unwrap();

    open(
        &conn,
        &mut ws_main,
        vec![
            depot_path("//game/main/a.txt"),
            depot_path("//game/main/b.txt"),
        ],
        OpenAction::Edit,
    )
    .await;
    open(
        &conn,
        &mut ws_main,
        vec![depot_path("//game/main/c.txt")],
        OpenAction::Delete,
    )
    .await;
    client.sandbox().write("main/a.txt", "A\n");
    client
        .sandbox()
        .write("main/b.txt", "1\n2\n3\n4\n5\n6\n7\nEIGHT\n");
    ws_main
        .submit(
            &conn,
            &client.cache(),
            LocalClId::DEFAULT,
            Some("main work".into()),
        )
        .await
        .unwrap();

    let err = conn
        .call(&Request::CopyUp {
            stream: dev.to_string(),
            description: "too early".into(),
        })
        .await;
    assert!(matches!(err, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Conflict));

    ws_dev.sync(&conn, &client.cache(), None).await.unwrap();
    let report = ws_dev.merge(&conn, &client.cache(), None).await.unwrap();
    let cl = report.changelist.expect("something to merge");
    assert_eq!(report.opened.len(), 3);
    assert_eq!(report.conflicts, vec![depot_path("//game/dev/b.txt")]);
    assert_eq!(client.sandbox().read("dev/a.txt"), b"A\n");
    assert!(!client.sandbox().exists("dev/c.txt"));
    ws_dev
        .resolve(&conn, &client.cache(), &[], kelp_client::ResolveMode::Auto)
        .await
        .unwrap();
    assert_eq!(
        client.sandbox().read("dev/b.txt"),
        b"ONE\n2\n3\n4\n5\n6\n7\nEIGHT\n"
    );
    ws_dev
        .submit(&conn, &client.cache(), cl, None)
        .await
        .unwrap();

    ws_dev.sync(&conn, &client.cache(), None).await.unwrap();
    assert!(
        ws_dev
            .merge(&conn, &client.cache(), None)
            .await
            .unwrap()
            .changelist
            .is_none(),
        "nothing left to merge"
    );
    conn.call(&Request::CopyUp {
        stream: dev.to_string(),
        description: "ship dev".into(),
    })
    .await
    .unwrap();
    ws_main.sync(&conn, &client.cache(), None).await.unwrap();
    assert_eq!(
        client.sandbox().read("main/b.txt"),
        b"ONE\n2\n3\n4\n5\n6\n7\nEIGHT\n"
    );
}
