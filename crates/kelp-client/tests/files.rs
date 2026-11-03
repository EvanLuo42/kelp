use kelp_client::{Connection, Workspace, read_file, render_diff};
use kelp_core::LocalClId;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

async fn submit(ws: &mut Workspace, conn: &Connection, client: &TestClient) -> u64 {
    ws.reconcile(conn).await.unwrap();
    ws.submit(conn, &client.cache(), LocalClId::DEFAULT, Some("c".into()))
        .await
        .unwrap()
}

#[tokio::test]
async fn status_remembers_files_it_hashed_unchanged() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "same");
    submit(&mut ws, &conn, &client).await;

    // Touched, as by a build tool rewriting the same bytes.
    let abs = client.sandbox().path("ws/a.txt");
    let touched = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&abs)
        .unwrap()
        .set_modified(touched)
        .unwrap();
    let a = depot_path("//game/main/a.txt");
    let disk = std::fs::metadata(&abs)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    assert_ne!(ws.state().have[&a].mtime, disk);

    let status = ws.refresh_status(None).unwrap();
    assert!(!status.has_unopened_changes(), "{status:?}");
    let reopened = Workspace::open(ws.root()).unwrap();
    assert_eq!(
        reopened.state().have[&a].mtime,
        disk,
        "the next status trusts the time instead of hashing"
    );

    client.sandbox().write("ws/a.txt", "diff");
    let status = ws.refresh_status(None).unwrap();
    assert_eq!(status.modified, vec![a]);
}

#[tokio::test]
async fn diff_shows_opened_and_unopened_changes() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "one\ntwo\n");
    client.sandbox().write("ws/b.txt", "keep\n");
    submit(&mut ws, &conn, &client).await;

    let a = depot_path("//game/main/a.txt");
    ws.open_files(
        &conn,
        std::slice::from_ref(&a),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    client.sandbox().write("ws/a.txt", "one\nTWO\n");
    client.sandbox().write("ws/b.txt", "changed\n");
    let diffs = ws.diff(&conn, &client.cache(), &[]).await.unwrap();
    assert_eq!(diffs.len(), 2);
    let text = render_diff(&diffs[0]);
    assert!(text.contains("-two") && text.contains("+TWO"), "{text}");

    let only_a = ws.diff(&conn, &client.cache(), &[a]).await.unwrap();
    assert_eq!(only_a.len(), 1);
}

#[tokio::test]
async fn clean_restores_unopened_files_and_removes_untracked_ones() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "original");
    client.sandbox().write("ws/b.txt", "gone");
    client.sandbox().write("ws/opened.txt", "base");
    submit(&mut ws, &conn, &client).await;

    let opened = depot_path("//game/main/opened.txt");
    ws.open_files(
        &conn,
        std::slice::from_ref(&opened),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    client.sandbox().write("ws/opened.txt", "my work");
    client.sandbox().write("ws/a.txt", "scribbled");
    std::fs::remove_file(client.sandbox().path("ws/b.txt")).unwrap();
    client.sandbox().write("ws/junk.tmp", "junk");

    let preview = ws.clean(&conn, &client.cache(), &[], true).await.unwrap();
    assert_eq!(preview.restored.len(), 2);
    assert!(
        client.sandbox().exists("ws/junk.tmp"),
        "a dry run changes nothing"
    );

    ws.clean(&conn, &client.cache(), &[], false).await.unwrap();
    assert_eq!(client.sandbox().read("ws/a.txt"), b"original");
    assert_eq!(client.sandbox().read("ws/b.txt"), b"gone");
    assert!(!client.sandbox().exists("ws/junk.tmp"));
    assert_eq!(
        client.sandbox().read("ws/opened.txt"),
        b"my work",
        "opened files stay"
    );
    assert!(ws.status().unwrap().modified.is_empty());
}

#[tokio::test]
async fn files_can_be_read_as_of_a_changelist() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "v1");
    let first = submit(&mut ws, &conn, &client).await;
    let a = depot_path("//game/main/a.txt");
    ws.open_files(
        &conn,
        std::slice::from_ref(&a),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    client.sandbox().write("ws/a.txt", "v2");
    submit(&mut ws, &conn, &client).await;

    let cache = client.cache();
    assert_eq!(read_file(&conn, &cache, &a, None).await.unwrap(), b"v2");
    assert_eq!(
        read_file(&conn, &cache, &a, Some(first)).await.unwrap(),
        b"v1"
    );
    assert!(
        read_file(&conn, &cache, &depot_path("//game/main/none"), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn annotate_names_the_change_behind_each_line() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    let a = depot_path("//game/main/a.txt");
    client.sandbox().write("ws/a.txt", "one\ntwo\n");
    let first = submit(&mut ws, &conn, &client).await;
    let mut edit = async |text: &str| {
        ws.open_files(
            &conn,
            std::slice::from_ref(&a),
            OpenAction::Edit,
            LocalClId::DEFAULT,
        )
        .await
        .unwrap();
        client.sandbox().write("ws/a.txt", text);
        submit(&mut ws, &conn, &client).await
    };
    let second = edit("one\nTWO\n").await;
    let third = edit("one\nTWO\nthree\n").await;
    let lines = kelp_client::annotate(&conn, &client.cache(), &a)
        .await
        .unwrap();
    let got: Vec<(u64, &str)> = lines.iter().map(|l| (l.cl, l.text.as_str())).collect();
    assert_eq!(
        got,
        [(first, "one\n"), (second, "TWO\n"), (third, "three\n")]
    );
}
