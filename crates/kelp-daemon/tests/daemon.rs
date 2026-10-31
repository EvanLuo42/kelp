use std::sync::Mutex;

use kelp_client::ipc::{call, call_with};
use kelp_client::ops::{Op, Outcome};
use kelp_client::progress::{Phase, Update};
use kelp_core::LocalClId;
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

#[tokio::test(flavor = "multi_thread")]
async fn operations_run_in_the_daemon() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server.add_user("alice").await;
    let client = TestClient::new();
    client.login(&server, "alice").await;
    client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.start_daemon().await;
    let (home, cwd) = (client.home(), client.workspace_root("ws"));

    assert_eq!(call(home, &cwd, Op::Ping).await.unwrap(), Outcome::Done);
    client
        .sandbox()
        .write("ws/Source/main.rs", "fn main() {}\n");
    let Outcome::Opened(opened) = call(home, &cwd, Op::Reconcile).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        opened,
        vec![(depot_path("//game/main/Source/main.rs"), OpenAction::Add)]
    );
    let submit = Op::Submit {
        cl: LocalClId::DEFAULT,
        message: Some("from kelpd".into()),
        fixes: Vec::new(),
    };
    assert_eq!(
        call(home, &cwd, submit).await.unwrap(),
        Outcome::Submitted(1)
    );

    // A second machine syncs through its own daemon.
    let other = TestClient::new();
    other.login(&server, "alice").await;
    other.create_workspace(&server, "ws", &depot_path("//game/main"));
    other.start_daemon().await;
    let Outcome::Synced(report) = call(
        other.home(),
        &other.workspace_root("ws"),
        Op::Sync { at: None },
    )
    .await
    .unwrap() else {
        panic!()
    };
    assert_eq!(report.cl, 1);
    assert_eq!(other.sandbox().read("ws/Source/main.rs"), b"fn main() {}\n");

    let err = call(home, &client.sandbox().dir("elsewhere"), Op::Status)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not inside a Kelp workspace"),
        "{err}"
    );
    call(home, &cwd, Op::Shutdown).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn progress_streams_before_the_outcome() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server.add_user("alice").await;
    let client = TestClient::new();
    client.login(&server, "alice").await;
    client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.start_daemon().await;
    let (home, cwd) = (client.home(), client.workspace_root("ws"));
    for i in 0..20 {
        client
            .sandbox()
            .write(&format!("ws/Content/{i}.txt"), format!("file {i}\n"));
    }
    call(home, &cwd, Op::Reconcile).await.unwrap();
    let updates = Mutex::new(Vec::new());
    let record = |u: Update| updates.lock().unwrap().push(u);
    let submit = Op::Submit {
        cl: LocalClId::DEFAULT,
        message: Some("many files".into()),
        fixes: Vec::new(),
    };
    call_with(home, &cwd, submit, &record).await.unwrap();
    let last_read = updates
        .lock()
        .unwrap()
        .iter()
        .rfind(|u| u.phase == Phase::Read)
        .copied()
        .expect("reading reported");
    assert_eq!((last_read.items, last_read.total_items), (20, 20));

    let other = TestClient::new();
    other.login(&server, "alice").await;
    other.create_workspace(&server, "ws", &depot_path("//game/main"));
    other.start_daemon().await;
    updates.lock().unwrap().clear();
    let sync = Op::Sync { at: None };
    call_with(other.home(), &other.workspace_root("ws"), sync, &record)
        .await
        .unwrap();
    let updates = updates.into_inner().unwrap();
    let finished = |phase| {
        updates
            .iter()
            .any(|u| u.phase == phase && u.is_finished() && u.total_items > 0)
    };
    assert!(finished(Phase::Download), "{updates:?}");
    let write = updates.iter().rfind(|u| u.phase == Phase::Write).unwrap();
    assert_eq!((write.items, write.bytes), (20, write.total_bytes.unwrap()));
    call(home, &cwd, Op::Shutdown).await.unwrap();
    call(other.home(), &other.workspace_root("ws"), Op::Shutdown)
        .await
        .unwrap();
}
