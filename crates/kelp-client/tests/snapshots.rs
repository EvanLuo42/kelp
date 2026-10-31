use std::sync::Arc;

use kelp_client::{SnapshotSide, render_diff};
use kelp_core::LocalClId;
use kelp_testkit::ManualClock;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

#[tokio::test]
async fn snapshots_capture_restore_and_diff() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    let clock = Arc::new(ManualClock::new(1_000));
    ws.set_clock(clock.clone());
    client.sandbox().write("ws/a.txt", "one\n");
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("base".into()),
    )
    .await
    .unwrap();

    let cl = ws.pending_mut().create("work");
    let a = depot_path("//game/main/a.txt");
    ws.open_files(&conn, std::slice::from_ref(&a), OpenAction::Edit, cl)
        .await
        .unwrap();
    client.sandbox().write("ws/a.txt", "one\ntwo\n");
    client.sandbox().write("ws/b.txt", "new\n");
    ws.open_files(
        &conn,
        &[depot_path("//game/main/b.txt")],
        OpenAction::Add,
        cl,
    )
    .await
    .unwrap();
    let first = ws.snapshot(&client.cache(), cl, "two lines").await.unwrap();
    assert_eq!(ws.snapshot_by_id(first).unwrap().at, 1_000);

    client.sandbox().write("ws/a.txt", "one\ntwo\nthree\n");
    let diffs = ws
        .diff_snapshots(
            &client.cache(),
            SnapshotSide::Snapshot(first),
            SnapshotSide::Working,
        )
        .await
        .unwrap();
    assert_eq!(diffs.len(), 1);
    let rendered = render_diff(&diffs[0]);
    assert!(rendered.contains("+three"), "{rendered}");

    // Reverting everything takes an automatic snapshot first, so nothing is lost.
    ws.revert(
        &conn,
        &client.cache(),
        &[a.clone(), depot_path("//game/main/b.txt")],
    )
    .await
    .unwrap();
    assert_eq!(client.sandbox().read("ws/a.txt"), b"one\n");
    let auto = ws.state().snapshots.last().unwrap().clone();
    assert!(auto.automatic && auto.message.contains("revert"));

    ws.restore(&conn, &client.cache(), auto.id).await.unwrap();
    assert_eq!(client.sandbox().read("ws/a.txt"), b"one\ntwo\nthree\n");
    assert_eq!(ws.state().pending.action(&a), Some(OpenAction::Edit));
    assert_eq!(ws.state().pending.owner(&a), Some(cl));

    ws.restore(&conn, &client.cache(), first).await.unwrap();
    assert_eq!(client.sandbox().read("ws/a.txt"), b"one\ntwo\n");
    ws.delete_snapshot(first).unwrap();
    assert!(ws.snapshot_by_id(first).is_err());
}
