//! Commit groups. These tests need an S3 test server and skip themselves
//! without one; see `kelp_testkit::s3`.

use std::time::Duration;

use kelp_client::{Connection, Error};
use kelp_core::LocalClId;
use kelp_meta::db::FORMAT_VERSION;
use kelp_proto::rpc::RaftRpc;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::cluster::TestCluster;
use kelp_testkit::depot_path;
use kelp_testkit::net::{ADMIN, TestClient, TestServer};
use redb::{ReadableTable, TableDefinition};

/// Submits `files` to `//game/main` through `server` as the administrator.
async fn submit(server: &TestServer, files: &[(&str, &str)]) -> u64 {
    let conn = server.login(ADMIN).await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(server, "ws", &depot_path("//game/main"));
    ws.sync(&conn, &client.cache(), None).await.unwrap();
    for (path, text) in files {
        client.sandbox().write(&format!("ws/{path}"), text);
    }
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("change".into()),
    )
    .await
    .unwrap()
}

/// Waits until `server` has changelist `cl`, as its own database shows it.
async fn wait_for_change(server: &TestServer, cl: u64) {
    let conn: Connection = server.login(ADMIN).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match conn.call(&Request::Describe { cl }).await {
            Ok(Reply::Describe(_)) => return,
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => panic!("changelist {cl} never arrived: {other:?}"),
        }
    }
}

/// Syncs `//game/main` from `server` into a new workspace and returns the
/// text of `path`.
async fn read_through(server: &TestServer, path: &str) -> String {
    let conn = server.login(ADMIN).await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(server, "ws", &depot_path("//game/main"));
    ws.sync(&conn, &client.cache(), None).await.unwrap();
    String::from_utf8(client.sandbox().read(&format!("ws/{path}"))).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_through_any_member_reach_every_member() {
    let Some(cluster) = TestCluster::start(3).await else {
        return;
    };
    let leader = cluster.leader().await;
    let follower = (leader + 1) % 3;
    cluster.member(follower).create_depot("game").await;
    let cl = submit(cluster.member(follower), &[("a.txt", "hello")]).await;
    for i in 0..3 {
        wait_for_change(cluster.member(i), cl).await;
    }
    let other = (leader + 2) % 3;
    assert_eq!(read_through(cluster.member(other), "a.txt").await, "hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_group_carries_on_without_its_leader() {
    let Some(mut cluster) = TestCluster::start(3).await else {
        return;
    };
    let first = cluster.leader().await;
    cluster.member(first).create_depot("game").await;
    let before = submit(cluster.member(first), &[("a.txt", "one")]).await;
    cluster.stop_member(first).await;

    let second = cluster.leader().await;
    assert_ne!(first, second);
    let after = submit(cluster.member(second), &[("b.txt", "two")]).await;
    assert_eq!(after, before + 1, "no changelist was lost or repeated");

    cluster.start_member(first).await;
    wait_for_change(cluster.member(first), after).await;
    assert_eq!(read_through(cluster.member(first), "b.txt").await, "two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founder_brings_its_history_into_the_group() {
    let Some(mut cluster) = TestCluster::prepare(3) else {
        return;
    };
    cluster.start_alone(0).await;
    cluster.member(0).create_depot("game").await;
    let cl = submit(cluster.member(0), &[("old.txt", "history")]).await;
    cluster.stop_member(0).await;

    for i in 0..3 {
        cluster.start_member(i).await;
    }
    cluster.wait_for_voters().await;
    for i in 0..3 {
        wait_for_change(cluster.member(i), cl).await;
    }
    assert_eq!(read_through(cluster.member(2), "old.txt").await, "history");
    let next = submit(cluster.member(1), &[("new.txt", "group")]).await;
    assert_eq!(next, cl + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_only_listen_to_each_other() {
    let Some(cluster) = TestCluster::start(1).await else {
        return;
    };
    let stranger = Request::Raft {
        secret: "not the secret".into(),
        format: FORMAT_VERSION,
        rpc: RaftRpc::Vote,
        payload: Vec::new(),
    };
    let err = cluster
        .member(0)
        .anonymous()
        .await
        .call(&stranger)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
    let _ = cluster.secret();

    let plain = TestServer::start().await;
    let err = plain.anonymous().await.call(&stranger).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_with_another_database_format_are_refused() {
    let Some(cluster) = TestCluster::start(1).await else {
        return;
    };
    let newer = Request::Raft {
        secret: cluster.secret().into(),
        format: FORMAT_VERSION + 1,
        rpc: RaftRpc::Probe,
        payload: Vec::new(),
    };
    let err = cluster
        .member(0)
        .anonymous()
        .await
        .call(&newer)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e)
            if e.kind == ErrorKind::Invalid && e.message.contains("same Kelp version")),
        "{err}"
    );
}

const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");
const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");

/// Stamps the Raft log of a stopped member with an older database format,
/// as an earlier Kelp would have left it, and returns its last index.
fn age_raft_log(cluster: &TestCluster, i: usize) -> u64 {
    let db = redb::Database::create(cluster.data_dir(i).join("raft.redb")).unwrap();
    let txn = db.begin_write().unwrap();
    let older = postcard::to_allocvec(&(FORMAT_VERSION - 1)).unwrap();
    txn.open_table(STATE)
        .unwrap()
        .insert("format", older.as_slice())
        .unwrap();
    let last = txn
        .open_table(ENTRIES)
        .unwrap()
        .last()
        .unwrap()
        .map_or(0, |(k, _)| k.value());
    txn.commit().unwrap();
    last
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_log_of_an_older_format_gives_way_to_a_snapshot() {
    let Some(mut cluster) = TestCluster::start(1).await else {
        return;
    };
    cluster.member(0).create_depot("game").await;
    let cl = submit(cluster.member(0), &[("a.txt", "kept")]).await;
    cluster.stop_member(0).await;
    assert!(age_raft_log(&cluster, 0) > 0);

    cluster.start_member(0).await;
    cluster.leader().await;
    assert_eq!(read_through(cluster.member(0), "a.txt").await, "kept");
    let next = submit(cluster.member(0), &[("b.txt", "after")]).await;
    assert_eq!(next, cl + 1);
    assert!(
        cluster
            .data_dir(0)
            .join("raft-snapshot/current.redb")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unapplied_log_of_an_older_format_is_refused() {
    let Some(mut cluster) = TestCluster::start(1).await else {
        return;
    };
    cluster.member(0).create_depot("game").await;
    cluster.stop_member(0).await;
    let last = age_raft_log(&cluster, 0);
    {
        // An entry the member never applied, in an encoding this build
        // cannot read.
        let db = redb::Database::create(cluster.data_dir(0).join("raft.redb")).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(ENTRIES)
            .unwrap()
            .insert(last + 1, [0xff_u8; 8].as_slice())
            .unwrap();
        txn.commit().unwrap();
    }

    let err = cluster.try_start_member(0).unwrap_err().to_string();
    assert!(err.contains("run the previous Kelp version"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founder_with_a_new_disk_rejoins_instead_of_founding_again() {
    let Some(mut cluster) = TestCluster::start(3).await else {
        return;
    };
    let leader = cluster.leader().await;
    cluster.member(leader).create_depot("game").await;
    let cl = submit(cluster.member(leader), &[("a.txt", "kept")]).await;
    cluster.stop_member(0).await;
    cluster.wipe_member(0);
    cluster.start_member(0).await;
    wait_for_change(cluster.member(0), cl).await;
    assert_eq!(read_through(cluster.member(0), "a.txt").await, "kept");
    let next = submit(cluster.member(0), &[("b.txt", "one group")]).await;
    assert_eq!(next, cl + 1);
}
