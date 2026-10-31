//! Regression tests for ways a workspace could lose someone's work.

use std::collections::BTreeMap;

use kelp_client::{Connection, Error, Workspace, artifact};
use kelp_core::{ContentId, DepotPath, LocalClId};
use kelp_proto::Request;
use kelp_proto::rpc::StreamKind;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_testkit::{depot_path, pseudo_random};
use kelp_workspace::{ArtifactMount, Config, OpenAction};

struct Setup {
    server: TestServer,
    conn: Connection,
    client: TestClient,
    ws: Workspace,
}

/// A workspace of `//game/main` that has submitted `files`.
async fn setup(files: &[(&str, &str)]) -> Setup {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    for (path, text) in files {
        client.sandbox().write(&format!("ws/{path}"), text);
    }
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("base".into()),
    )
    .await
    .unwrap();
    Setup {
        server,
        conn,
        client,
        ws,
    }
}

fn path(rel: &str) -> DepotPath {
    depot_path(&format!("//game/main/{rel}"))
}

async fn open(s: &mut Setup, rel: &str, action: OpenAction) {
    s.ws.open_files(&s.conn, &[path(rel)], action, LocalClId::DEFAULT)
        .await
        .unwrap();
}

fn would_overwrite(err: Error) -> bool {
    matches!(err, Error::WouldOverwrite(_))
}

#[tokio::test]
async fn unshelve_keeps_unopened_edits_and_untracked_files() {
    let mut s = setup(&[("a.txt", "a\n")]).await;
    open(&mut s, "a.txt", OpenAction::Edit).await;
    s.client.sandbox().write("ws/a.txt", "shelved\n");
    s.client.sandbox().write("ws/new.txt", "shelved new\n");
    s.ws.open_files(
        &s.conn,
        &[path("new.txt")],
        OpenAction::Add,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    let shelf =
        s.ws.shelve(&s.conn, &s.client.cache(), LocalClId::DEFAULT)
            .await
            .unwrap();
    s.ws.revert(
        &s.conn,
        &s.client.cache(),
        &[path("a.txt"), path("new.txt")],
    )
    .await
    .unwrap();

    s.client.sandbox().write("ws/a.txt", "my unopened edit\n");
    let err =
        s.ws.unshelve(&s.conn, &s.client.cache(), shelf, None)
            .await
            .unwrap_err();
    assert!(would_overwrite(err));
    assert_eq!(s.client.sandbox().read("ws/a.txt"), b"my unopened edit\n");

    s.client.sandbox().write("ws/a.txt", "a\n");
    s.client
        .sandbox()
        .write("ws/new.txt", "someone else's file\n");
    let err =
        s.ws.unshelve(&s.conn, &s.client.cache(), shelf, None)
            .await
            .unwrap_err();
    assert!(would_overwrite(err));
    assert_eq!(
        s.client.sandbox().read("ws/new.txt"),
        b"someone else's file\n"
    );

    let missing =
        s.ws.unshelve(&s.conn, &s.client.cache(), shelf, Some(LocalClId(42)))
            .await;
    assert!(missing.is_err());
    assert_eq!(
        s.client.sandbox().read("ws/new.txt"),
        b"someone else's file\n"
    );
}

#[tokio::test]
async fn merge_keeps_unopened_edits() {
    let mut s = setup(&[("a.txt", "a\n")]).await;
    let dev = depot_path("//game/dev");
    s.server
        .admin(Request::CreateStream {
            path: dev.to_string(),
            kind: StreamKind::Development,
            parent: Some("//game/main".into()),
        })
        .await;
    let mut ws_dev = s.client.create_workspace(&s.server, "dev", &dev);
    ws_dev.sync(&s.conn, &s.client.cache(), None).await.unwrap();
    open(&mut s, "a.txt", OpenAction::Edit).await;
    s.client.sandbox().write("ws/a.txt", "parent change\n");
    s.ws.submit(
        &s.conn,
        &s.client.cache(),
        LocalClId::DEFAULT,
        Some("p".into()),
    )
    .await
    .unwrap();

    s.client.sandbox().write("dev/a.txt", "unopened work\n");
    let err = ws_dev
        .merge(&s.conn, &s.client.cache(), None)
        .await
        .unwrap_err();
    assert!(would_overwrite(err));
    assert_eq!(s.client.sandbox().read("dev/a.txt"), b"unopened work\n");
    assert_eq!(
        ws_dev.state().pending.changelists().count(),
        1,
        "no merge changelist is left"
    );
}

#[tokio::test]
async fn a_failed_revert_keeps_its_snapshot() {
    let mut s = setup(&[("a.txt", "a\n"), ("b.txt", "b\n")]).await;
    open(&mut s, "a.txt", OpenAction::Edit).await;
    open(&mut s, "b.txt", OpenAction::Edit).await;
    s.client.sandbox().write("ws/a.txt", "precious a\n");
    s.client.sandbox().write("ws/b.txt", "precious b\n");
    // The base of b.txt is no longer cached, so the revert fails part way.
    let hex = ContentId::of(b"b\n").to_string();
    let chunk =
        s.client
            .home()
            .dir()
            .join(format!("cache/chunks/{}/{}/{hex}", &hex[..2], &hex[2..4]));
    std::fs::remove_file(chunk).unwrap();
    let paths = [path("a.txt"), path("b.txt")];
    assert!(
        s.ws.revert(&s.conn, &s.client.cache(), &paths)
            .await
            .is_err()
    );

    let reopened = Workspace::open(&s.client.workspace_root("ws")).unwrap();
    let snapshot = reopened
        .state()
        .snapshots
        .last()
        .expect("the automatic snapshot was saved");
    let saved = snapshot
        .files
        .iter()
        .find(|f| f.path == path("a.txt"))
        .unwrap();
    assert_eq!(
        s.client
            .cache()
            .read(&saved.content.unwrap().content)
            .await
            .unwrap(),
        b"precious a\n"
    );
}

#[tokio::test]
async fn deleting_an_opened_file_is_refused() {
    let mut s = setup(&[("a.txt", "a\n")]).await;
    open(&mut s, "a.txt", OpenAction::Edit).await;
    s.client.sandbox().write("ws/a.txt", "edits\n");
    let err =
        s.ws.open_files(
            &s.conn,
            &[path("a.txt")],
            OpenAction::Delete,
            LocalClId::DEFAULT,
        )
        .await;
    assert!(err.is_err());
    assert_eq!(s.client.sandbox().read("ws/a.txt"), b"edits\n");
}

#[tokio::test]
async fn restore_keeps_edits_made_after_the_snapshot() {
    let mut s = setup(&[("a.txt", "a\n")]).await;
    open(&mut s, "a.txt", OpenAction::Edit).await;
    s.client.sandbox().write("ws/a.txt", "snapshotted\n");
    let id =
        s.ws.snapshot(&s.client.cache(), LocalClId::DEFAULT, "wip")
            .await
            .unwrap();
    s.ws.revert(&s.conn, &s.client.cache(), &[path("a.txt")])
        .await
        .unwrap();
    s.client
        .sandbox()
        .write("ws/a.txt", "newer unopened work\n");
    let err =
        s.ws.restore(&s.conn, &s.client.cache(), id)
            .await
            .unwrap_err();
    assert!(would_overwrite(err));
    assert_eq!(
        s.client.sandbox().read("ws/a.txt"),
        b"newer unopened work\n"
    );
}

#[tokio::test]
async fn sync_finishes_what_an_interrupted_sync_wrote() {
    let mut s = setup(&[("a.txt", "v1\n")]).await;
    let other = TestClient::new();
    let mut ws2 = other.create_workspace(&s.server, "ws", &depot_path("//game/main"));
    ws2.sync(&s.conn, &other.cache(), None).await.unwrap();
    ws2.open_files(
        &s.conn,
        &[path("a.txt")],
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    other.sandbox().write("ws/a.txt", "v2\n");
    ws2.submit(
        &s.conn,
        &other.cache(),
        LocalClId::DEFAULT,
        Some("v2".into()),
    )
    .await
    .unwrap();

    // An interrupted sync wrote v2 but did not record it.
    s.client.sandbox().write("ws/a.txt", "v2\n");
    let report = s.ws.sync(&s.conn, &s.client.cache(), None).await.unwrap();
    assert_eq!(report.updated, [path("a.txt")]);
    assert!(s.ws.status().unwrap().modified.is_empty());
}

#[tokio::test]
async fn artifact_directories_do_not_swallow_other_files() {
    let s = setup(&[("a.txt", "a\n")]).await;
    let cl = s.ws.state().have_cl;
    s.client
        .sandbox()
        .write("build/tool.exe", pseudo_random(1000, 1));
    artifact::publish(
        &s.conn,
        &s.client.cache(),
        &depot_path("//game/main"),
        cl,
        "tools",
        &BTreeMap::new(),
        &s.client.sandbox().path("build"),
    )
    .await
    .unwrap();
    let mut config = Config::new("ws2", s.server.addr(), &depot_path("//game/main"));
    config.artifacts.push(ArtifactMount {
        name: "tools".into(),
        to: "Tools".into(),
        stream: None,
        exact: false,
        tags: BTreeMap::new(),
    });
    let mut ws2 = Workspace::create(&s.client.sandbox().dir("ws2"), config).unwrap();
    s.client
        .sandbox()
        .write("ws2/Tools/tool.exe", "my own build\n");
    assert!(ws2.sync(&s.conn, &s.client.cache(), None).await.is_err());
    assert_eq!(
        s.client.sandbox().read("ws2/Tools/tool.exe"),
        b"my own build\n"
    );
}
