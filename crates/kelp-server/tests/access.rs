//! Protections hold for requests that name trees and streams directly.

use kelp_client::{Connection, Error};
use kelp_core::{LocalClId, TreeId};
use kelp_proto::rpc::StreamKind;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

/// Starts a server where alice submitted `Secret/plan.txt` and
/// `Art/tree.txt` to `//game/main`, and bob may not read `Secret`.
async fn secret_depot() -> (TestServer, Connection, Connection) {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let (alice, bob) = (server.add_user("alice").await, server.add_user("bob").await);
    let table = "write * //...\nnone user:bob //game/main/Secret/...\n";
    server
        .admin(Request::SetProtections {
            source: table.into(),
        })
        .await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/Secret/plan.txt", "plan");
    client.sandbox().write("ws/Art/tree.txt", "tree");
    ws.reconcile(&alice).await.unwrap();
    ws.submit(&alice, &client.cache(), LocalClId::DEFAULT, Some("all".into()))
        .await
        .unwrap();
    client.sandbox().write("ws/Art/tree.txt", "a taller tree");
    ws.reconcile(&alice).await.unwrap();
    ws.submit(&alice, &client.cache(), LocalClId::DEFAULT, Some("edit".into()))
        .await
        .unwrap();
    (server, alice, bob)
}

fn diff(stream: &str, from: Option<TreeId>, to: TreeId) -> Request {
    Request::DiffTree {
        stream: stream.into(),
        from,
        to,
        paths: vec!["Secret/plan.txt".into()],
    }
}

fn remote_kind(result: Result<Reply, Error>) -> ErrorKind {
    match result {
        Err(Error::Remote(e)) => e.kind,
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test]
async fn trees_of_other_streams_cannot_be_diffed() {
    let (server, _alice, bob) = secret_depot().await;
    server.create_depot("other").await;
    let head = bob.resolve_stream("//game/main", None).await.unwrap();
    let first = bob.resolve_stream("//game/main", Some(1)).await.unwrap();

    // Within its own stream, a tree is filtered by that stream's rules.
    for from in [None, Some(first.root), Some(head.root)] {
        let Reply::TreeDiff { changes, lookups } =
            bob.call(&diff("//game/main", from, head.root)).await.unwrap()
        else {
            panic!("unexpected reply");
        };
        assert!(changes.iter().all(|d| !d.path.starts_with("Secret")));
        assert_eq!(lookups, [None]);
    }

    // Named as a tree of a stream without that rule, it would show the
    // secret files.
    assert_eq!(
        remote_kind(bob.call(&diff("//other/main", None, head.root)).await),
        ErrorKind::Invalid
    );
    let other = bob.resolve_stream("//other/main", None).await.unwrap();
    assert_eq!(
        remote_kind(
            bob.call(&diff("//other/main", Some(head.root), other.root))
                .await
        ),
        ErrorKind::Invalid
    );
    bob.call(&diff("//other/main", None, other.root))
        .await
        .unwrap();
}

#[tokio::test]
async fn task_streams_need_the_whole_parent_readable() {
    let (_server, alice, bob) = secret_depot().await;
    let task = |path: &str| Request::CreateStream {
        path: path.into(),
        kind: StreamKind::Task,
        parent: Some("//game/main".into()),
    };
    assert_eq!(
        remote_kind(bob.call(&task("//game/bob-task")).await),
        ErrorKind::Forbidden,
        "the copy would show bob the secret files under paths he may read"
    );
    alice.call(&task("//game/alice-task")).await.unwrap();
}
