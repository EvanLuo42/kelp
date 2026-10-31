use std::path::Path;

use kelp_client::Workspace;
use kelp_client::ipc::call;
use kelp_client::ops::{Op, Outcome};
use kelp_core::LocalClId;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

/// Asserts that kelpd's watched status equals a full scan of the disk.
async fn assert_status_matches(client: &TestClient, cwd: &Path) -> kelp_client::Status {
    let Outcome::Status { status, .. } = call(client.home(), cwd, Op::Status).await.unwrap() else {
        panic!("expected status");
    };
    let scanned = Workspace::open(cwd).unwrap().status().unwrap();
    assert_eq!(status, scanned);
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn watched_status_tracks_every_change() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server.add_user("alice").await;
    let client = TestClient::new();
    client.login(&server, "alice").await;
    client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.start_daemon().await;
    let cwd = client.workspace_root("ws");
    let sb = client.sandbox();

    for i in 0..20 {
        sb.write(
            &format!("ws/Content/Level{}/actor{i}.txt", i % 3),
            format!("{i}"),
        );
    }
    sb.write("ws/notes.txt", "notes");
    call(client.home(), &cwd, Op::Reconcile).await.unwrap();
    let submit = Op::Submit {
        cl: LocalClId::DEFAULT,
        message: Some("base".into()),
        fixes: Vec::new(),
    };
    call(client.home(), &cwd, submit).await.unwrap();
    let status = assert_status_matches(&client, &cwd).await;
    assert!(!status.has_unopened_changes());

    sb.write("ws/notes.txt", "edited");
    sb.write("ws/New/Deep/Dir/file.txt", "new");
    let status = assert_status_matches(&client, &cwd).await;
    assert_eq!(status.modified, vec![depot_path("//game/main/notes.txt")]);
    assert_eq!(
        status.added,
        vec![depot_path("//game/main/New/Deep/Dir/file.txt")]
    );

    std::fs::remove_dir_all(sb.path("ws/Content/Level0")).unwrap();
    std::fs::rename(sb.path("ws/Content/Level1"), sb.path("ws/Content/Renamed")).unwrap();
    let status = assert_status_matches(&client, &cwd).await;
    assert_eq!(status.deleted.len(), 7 + 7);
    assert_eq!(status.added.len(), 1 + 7);

    sb.write("ws/Saved/log.txt", "log");
    sb.write("ws/.kelpignore", "/Saved/\n");
    let status = assert_status_matches(&client, &cwd).await;
    assert!(
        !status
            .added
            .contains(&depot_path("//game/main/Saved/log.txt"))
    );
    sb.write("ws/.kelpignore", "");
    let status = assert_status_matches(&client, &cwd).await;
    assert!(
        status
            .added
            .contains(&depot_path("//game/main/Saved/log.txt"))
    );

    // Operations that change the workspace state without touching files.
    call(client.home(), &cwd, Op::Reconcile).await.unwrap();
    let status = assert_status_matches(&client, &cwd).await;
    assert!(!status.has_unopened_changes());
    let submit = Op::Submit {
        cl: LocalClId::DEFAULT,
        message: Some("everything".into()),
        fixes: Vec::new(),
    };
    call(client.home(), &cwd, submit).await.unwrap();
    let status = assert_status_matches(&client, &cwd).await;
    assert!(status.opened.is_empty() && !status.has_unopened_changes());
}
