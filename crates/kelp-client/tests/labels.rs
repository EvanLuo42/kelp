use kelp_client::{Connection, Error, Workspace};
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

async fn submit(ws: &mut Workspace, conn: &Connection, client: &TestClient) -> u64 {
    ws.reconcile(conn).await.unwrap();
    ws.submit(conn, &client.cache(), LocalClId::DEFAULT, Some("c".into()))
        .await
        .unwrap()
}

#[tokio::test]
async fn labels_name_a_point_in_a_stream() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let alice = server.add_user("alice").await;
    let bob = server.add_user("bob").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "v1");
    let first = submit(&mut ws, &alice, &client).await;
    let set = |cl: Option<u64>| Request::SetLabel {
        name: "ship-1.0".into(),
        stream: "//game/main".into(),
        cl,
        description: "first release".into(),
    };
    alice.call(&set(None)).await.unwrap();
    let Reply::Label(label) = bob
        .call(&Request::GetLabel {
            name: "Ship-1.0".into(),
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!((label.cl, label.owner.as_str()), (first, "alice"));

    let err = bob.call(&set(Some(first))).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
    assert!(bob.call(&set(Some(first + 50))).await.is_err());
    server
        .admin(Request::DeleteLabel {
            name: "ship-1.0".into(),
        })
        .await;
    let Reply::Labels(labels) = alice.call(&Request::ListLabels).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert!(labels.is_empty());
}

#[tokio::test]
async fn a_partial_sync_leaves_the_rest_for_later() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let (a, b) = (TestClient::new(), TestClient::new());
    let stream = depot_path("//game/main");
    let mut ws_a = a.create_workspace(&server, "ws", &stream);
    a.sandbox().write("ws/Code/main.rs", "code");
    a.sandbox().write("ws/Content/map.bin", "map");
    submit(&mut ws_a, &conn, &a).await;

    let mut ws_b = b.create_workspace(&server, "ws", &stream);
    let code = depot_path("//game/main/Code");
    ws_b.sync_paths(&conn, &b.cache(), None, &[code])
        .await
        .unwrap();
    assert!(b.sandbox().exists("ws/Code/main.rs"));
    assert!(!b.sandbox().exists("ws/Content/map.bin"), "not asked for");

    ws_b.sync(&conn, &b.cache(), None).await.unwrap();
    assert_eq!(
        b.sandbox().read("ws/Content/map.bin"),
        b"map",
        "a full sync catches up"
    );
}
