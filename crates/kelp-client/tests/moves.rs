use kelp_client::{Connection, Workspace};
use kelp_core::LocalClId;
use kelp_proto::{Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

async fn submit(ws: &mut Workspace, conn: &Connection, client: &TestClient) -> u64 {
    ws.submit(conn, &client.cache(), LocalClId::DEFAULT, Some("c".into()))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_moved_file_keeps_its_history() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let (a, b) = (TestClient::new(), TestClient::new());
    let stream = depot_path("//game/main");
    let mut ws = a.create_workspace(&server, "ws", &stream);
    a.sandbox().write("ws/Art/hero.txt", "v1");
    ws.reconcile(&conn).await.unwrap();
    submit(&mut ws, &conn, &a).await;
    let old = depot_path("//game/main/Art/hero.txt");
    let new = depot_path("//game/main/Art/Characters/hero.txt");
    ws.open_files(
        &conn,
        std::slice::from_ref(&old),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    a.sandbox().write("ws/Art/hero.txt", "v2");
    ws.move_file(&conn, &old, &new, LocalClId::DEFAULT)
        .await
        .unwrap();
    assert!(!a.sandbox().exists("ws/Art/hero.txt"));
    assert_eq!(
        a.sandbox().read("ws/Art/Characters/hero.txt"),
        b"v2",
        "the edit moves along"
    );
    submit(&mut ws, &conn, &a).await;

    let Reply::Filelog(revs) = conn
        .call(&Request::Filelog {
            path: new.to_string(),
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    let paths: Vec<&str> = revs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, [new.as_str(), old.as_str()]);

    let mut ws_b = b.create_workspace(&server, "ws", &stream);
    ws_b.sync(&conn, &b.cache(), None).await.unwrap();
    assert_eq!(b.sandbox().read("ws/Art/Characters/hero.txt"), b"v2");
    assert!(!b.sandbox().exists("ws/Art/hero.txt"));
}

#[tokio::test]
async fn reverting_one_half_of_a_move_reverts_both() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let a = TestClient::new();
    let mut ws = a.create_workspace(&server, "ws", &depot_path("//game/main"));
    a.sandbox().write("ws/a.txt", "a");
    ws.reconcile(&conn).await.unwrap();
    submit(&mut ws, &conn, &a).await;
    let (from, to) = (
        depot_path("//game/main/a.txt"),
        depot_path("//game/main/b.txt"),
    );
    ws.move_file(&conn, &from, &to, LocalClId::DEFAULT)
        .await
        .unwrap();
    ws.revert(&conn, &a.cache(), std::slice::from_ref(&to))
        .await
        .unwrap();
    assert_eq!(a.sandbox().read("ws/a.txt"), b"a");
    assert!(!a.sandbox().exists("ws/b.txt"));
    assert!(ws.state().pending.action(&from).is_none());
    assert!(ws.state().moves.is_empty());
}
