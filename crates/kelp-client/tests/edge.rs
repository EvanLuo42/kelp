use std::time::Duration;

use kelp_client::{Connection, Error};
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Reply, Request, ServerRole};
use kelp_testkit::net::{TestClient, TestServer};
use kelp_testkit::{depot_path, pseudo_random};
use kelp_workspace::OpenAction;

fn stream() -> kelp_core::DepotPath {
    depot_path("//game/main")
}

/// Waits until `conn`'s server has changelist `cl` of the stream.
async fn wait_for_cl(conn: &Connection, cl: u64) {
    for _ in 0..500 {
        let request = Request::ResolveStream {
            stream: stream().to_string(),
            at: None,
        };
        if let Ok(Reply::Stream(head)) = conn.call(&request).await
            && head.cl >= cl
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("changelist {cl} was not replicated");
}

#[tokio::test]
async fn an_edge_serves_reads_and_forwards_writes() {
    let commit = TestServer::start().await;
    commit.create_depot("game").await;
    commit.add_user("alice").await;
    let bob_conn = commit.add_user("bob").await;
    let edge = commit.start_edge("edge-eu").await;

    let alice_conn = edge.login("alice").await;
    assert_eq!(alice_conn.welcome().role, ServerRole::Edge);
    let (alice, bob) = (TestClient::new(), TestClient::new());
    let mut ws_a = alice.create_workspace(&edge, "ws", &stream());
    let mut ws_b = bob.create_workspace(&commit, "ws", &stream());

    let big = pseudo_random(2 << 20, 1);
    alice.sandbox().write("ws/Content/Hero.uasset", &big);
    alice.sandbox().write("ws/Source/main.rs", "fn main() {}\n");
    ws_a.reconcile(&alice_conn).await.unwrap();
    let cl1 = ws_a
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("import".into()),
        )
        .await
        .unwrap();
    let Reply::Describe(change) = alice_conn
        .call(&Request::Describe { cl: cl1 })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(
        change.summary.author, "alice",
        "the edge has replicated the submit before replying"
    );

    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert_eq!(bob.sandbox().read("ws/Content/Hero.uasset"), big);
    let main = depot_path("//game/main/Source/main.rs");
    ws_b.open_files(
        &bob_conn,
        std::slice::from_ref(&main),
        OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    bob.sandbox()
        .write("ws/Source/main.rs", "fn main() { run() }\n");
    let cl2 = ws_b
        .submit(
            &bob_conn,
            &bob.cache(),
            LocalClId::DEFAULT,
            Some("run".into()),
        )
        .await
        .unwrap();

    wait_for_cl(&alice_conn, cl2).await;
    ws_a.sync(&alice_conn, &alice.cache(), None).await.unwrap();
    assert_eq!(
        alice.sandbox().read("ws/Source/main.rs"),
        b"fn main() { run() }\n"
    );

    let carol = TestClient::new();
    let carol_conn = edge.add_user("carol").await;
    let mut ws_c = carol.create_workspace(&edge, "ws", &stream());
    ws_c.sync(&carol_conn, &carol.cache(), None).await.unwrap();
    assert_eq!(
        carol.sandbox().read("ws/Content/Hero.uasset"),
        big,
        "served from the edge's cache"
    );

    let Reply::AuditVerified { entries, .. } = edge.admin(Request::VerifyAudit).await else {
        panic!("unexpected reply");
    };
    assert!(entries > 0, "the replicated audit chain verifies");
}

#[tokio::test]
async fn only_edge_accounts_read_the_log() {
    let commit = TestServer::start().await;
    let alice = commit.add_user("alice").await;
    let request = Request::ReadLog {
        from: 1,
        limit: 10,
        wait_ms: 0,
    };
    let err = alice.call(&request).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );

    let token = commit.edge_token("edge-us").await;
    let edge_conn = Connection::connect(
        commit.addr(),
        kelp_client::Trust::Pinned(commit.fingerprint().to_owned()),
        kelp_proto::Auth::Bearer(token),
    )
    .await
    .unwrap();
    let Reply::Log { entries, head, .. } = edge_conn.call(&request).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(entries.first().map(|e| e.index), Some(1));
    assert!(head >= entries.len() as u64);
    let submit = Request::CreateDepot { name: "x".into() };
    let err = edge_conn.call(&submit).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
}

#[tokio::test]
async fn an_edge_without_an_edge_account_stops_replicating() {
    let commit = TestServer::start().await;
    commit.add_user("alice").await;
    let Reply::Token { token, .. } = commit
        .admin(Request::IssueToken {
            user: "alice".into(),
            ttl_days: 1,
        })
        .await
    else {
        panic!("unexpected reply");
    };
    let edge = commit.start_edge_with(token);
    for _ in 0..500 {
        let login = Request::Login {
            user: "alice".into(),
            password: kelp_testkit::net::PASSWORD.into(),
            code: None,
        };
        match edge.anonymous().await.call(&login).await {
            Err(Error::Remote(e)) if e.kind == ErrorKind::Unavailable => {
                assert!(e.message.contains("edge account"), "{}", e.message);
                return;
            }
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    panic!("the edge kept serving");
}

#[tokio::test]
async fn an_edge_tries_its_upstreams_in_turn() {
    let commit = TestServer::start().await;
    commit.create_depot("game").await;
    let token = commit.edge_token("edge-eu").await;
    let dead = "127.0.0.1:9".to_owned();
    let edge = commit.start_edge_via(vec![dead, commit.addr().to_owned()], token);
    let conn = edge.add_user("alice").await;
    let Reply::Streams(streams) = conn.call(&Request::ListStreams).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(streams.len(), 1, "replicated through the second address");
}
