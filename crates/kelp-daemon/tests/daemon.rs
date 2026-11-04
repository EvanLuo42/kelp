use std::sync::Mutex;

use kelp_client::ipc::{IpcReply, IpcRequest, call, call_with, connect};
use kelp_client::ops::{Op, Outcome};
use kelp_client::progress::{Phase, Update};
use kelp_core::LocalClId;
use kelp_proto::MsgType;
use kelp_proto::io::{expect_frame, write_message};
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

/// Sends `op` as a client of `version` would, and returns kelpd's answer.
async fn call_as(home: &kelp_client::Home, version: &str, op: Op) -> IpcReply {
    let mut stream = connect(home).await.unwrap();
    let request = IpcRequest {
        version: version.to_owned(),
        cwd: home.dir().to_owned(),
        op,
    };
    write_message(&mut stream, MsgType::Request, &request)
        .await
        .unwrap();
    expect_frame(&mut stream).await.unwrap().decode().unwrap()
}

async fn wait_until_gone(home: &kelp_client::Home) {
    for _ in 0..500 {
        if connect(home).await.is_err() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("kelpd did not stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_of_another_version_can_only_stop_kelpd() {
    let client = TestClient::new();
    client.start_daemon().await;
    let home = client.home();
    assert!(matches!(
        call_as(home, "0.0.0-other", Op::Ping).await,
        IpcReply::VersionMismatch(v) if v == kelp_client::ipc::VERSION
    ));
    assert!(matches!(
        call_as(home, "0.0.0-other", Op::Shutdown).await,
        IpcReply::Ok(Outcome::Done)
    ));
    wait_until_gone(home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_that_leave_at_once_do_not_stop_kelpd() {
    let client = TestClient::new();
    client.start_daemon().await;
    let home = client.home();
    for _ in 0..50 {
        drop(connect(home).await.unwrap());
    }
    assert_eq!(
        call(home, home.dir(), Op::Ping).await.unwrap(),
        Outcome::Done
    );
    call(home, home.dir(), Op::Shutdown).await.unwrap();
    wait_until_gone(home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_kelpd_for_the_same_home_does_not_start() {
    let client = TestClient::new();
    client.start_daemon().await;
    let home = client.home();
    let second = kelp_daemon::serve(kelp_daemon::Config {
        home: home.dir().to_owned(),
        idle_timeout: None,
    })
    .await;
    assert!(second.is_err());
    assert_eq!(
        call(home, home.dir(), Op::Ping).await.unwrap(),
        Outcome::Done
    );
    call(home, home.dir(), Op::Shutdown).await.unwrap();
}
