use std::collections::BTreeMap;

use kelp_client::{Error, artifact};
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

fn kind(result: Result<Reply, Error>) -> ErrorKind {
    match result {
        Err(Error::Remote(e)) => e.kind,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn changes_link_issues_and_queries_follow_them() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let alice_conn = server.add_user("alice").await;
    let bob_conn = server.add_user("bob").await;
    let alice = TestClient::new();
    let stream = depot_path("//game/main");
    let mut ws = alice.create_workspace(&server, "ws", &stream);
    alice.sandbox().write("ws/swamp.map", "v1");
    ws.reconcile(&alice_conn).await.unwrap();
    let bad = ws
        .submit_fixing(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("x".into()),
            &["nope".into()],
        )
        .await
        .unwrap_err();
    assert!(matches!(bad, Error::Remote(ref e) if e.kind == ErrorKind::Invalid));
    let cl = ws
        .submit_fixing(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("Fix swamp loading, see GAME-4".into()),
            &["game-7".into()],
        )
        .await
        .unwrap();

    let Reply::Describe(d) = alice_conn.call(&Request::Describe { cl }).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(
        (d.summary.issues, d.summary.fixes),
        (
            vec!["GAME-4".to_owned(), "GAME-7".to_owned()],
            vec!["GAME-7".to_owned()]
        )
    );
    let Reply::Issue(detail) = bob_conn
        .call(&Request::GetIssue {
            key: "game-7".into(),
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(detail.key, "GAME-7");
    assert_eq!(detail.changes[0].cl, cl);
    assert_eq!(detail.reached, [("//game/main".to_owned(), cl)]);

    alice.sandbox().write("build/game.pak", "pak");
    let built = artifact::publish(
        &alice_conn,
        &alice.cache(),
        &stream,
        cl,
        "game",
        &BTreeMap::new(),
        &alice.sandbox().path("build"),
    )
    .await
    .unwrap();
    let info = artifact::get(&bob_conn, built).await.unwrap();
    let Reply::IssueKeys(keys) = bob_conn
        .call(&Request::IssuesIn {
            stream: info.stream,
            at: Some(info.cl),
            since: None,
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(
        keys,
        [
            ("GAME-4".to_owned(), vec![cl]),
            ("GAME-7".to_owned(), vec![cl])
        ]
    );

    let unlink = Request::LinkIssues {
        cl,
        add: vec![],
        remove: vec!["GAME-4".into()],
    };
    assert_eq!(kind(bob_conn.call(&unlink).await), ErrorKind::Forbidden);
    alice_conn.call(&unlink).await.unwrap();

    server
        .admin(Request::SetTypemap {
            depot: "game".into(),
            source: "[issues]\nprefixes = [\"ART\"]\n".into(),
        })
        .await;
    server
        .admin(Request::SetReviewRules {
            source: "issue //game/main/...".into(),
        })
        .await;
    let map = depot_path("//game/main/swamp.map");
    ws.open_files(
        &alice_conn,
        std::slice::from_ref(&map),
        kelp_workspace::OpenAction::Edit,
        LocalClId::DEFAULT,
    )
    .await
    .unwrap();
    alice.sandbox().write("ws/swamp.map", "v2");
    let blocked = ws
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("tweak GAME-5".into()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(blocked, Error::Remote(ref e) if e.kind == ErrorKind::Conflict && e.message.contains("linked issue")),
        "GAME is not a configured prefix: {blocked}"
    );
    let cl2 = ws
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("tweak for ART-2".into()),
        )
        .await
        .unwrap();
    let Reply::IssueKeys(keys) = bob_conn
        .call(&Request::IssuesIn {
            stream: "//game/main".into(),
            at: None,
            since: Some(cl),
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(keys, [("ART-2".to_owned(), vec![cl2])]);
}
