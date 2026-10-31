use kelp_client::{Connection, Error, Trust};
use kelp_core::LocalClId;
use kelp_proto::{Auth, ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{PASSWORD, TestClient, TestServer};

#[tokio::test]
async fn protections_hide_and_guard_paths() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let (alice_conn, bob_conn) = (server.add_user("alice").await, server.add_user("bob").await);
    server
        .admin(Request::SetGroup {
            name: "outsourcers".into(),
            members: vec!["bob".into()],
        })
        .await;
    let table = "write * //...\nnone group:outsourcers //game/main/Secret/...\nread group:outsourcers //game/main/Engine/...\n";
    server
        .admin(Request::SetProtections {
            source: table.into(),
        })
        .await;

    let (alice, bob) = (TestClient::new(), TestClient::new());
    let stream = depot_path("//game/main");
    let mut ws_a = alice.create_workspace(&server, "ws", &stream);
    let mut ws_b = bob.create_workspace(&server, "ws", &stream);
    for (path, text) in [
        ("ws/Secret/plan.txt", "plan"),
        ("ws/Engine/core.rs", "core"),
        ("ws/Art/tree.txt", "tree"),
    ] {
        alice.sandbox().write(path, text);
    }
    ws_a.reconcile(&alice_conn).await.unwrap();
    ws_a.submit(
        &alice_conn,
        &alice.cache(),
        LocalClId::DEFAULT,
        Some("all".into()),
    )
    .await
    .unwrap();

    ws_b.sync(&bob_conn, &bob.cache(), None).await.unwrap();
    assert!(!bob.sandbox().exists("ws/Secret/plan.txt"));
    assert!(bob.sandbox().exists("ws/Engine/core.rs"));
    assert!(bob.sandbox().exists("ws/Art/tree.txt"));
    let Reply::Describe(detail) = bob_conn.call(&Request::Describe { cl: 1 }).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        detail.files.len(),
        2,
        "hidden files are left out of describe"
    );

    let read_only = depot_path("//game/main/Engine/core.rs");
    let err = ws_b
        .open_files(
            &bob_conn,
            std::slice::from_ref(&read_only),
            kelp_workspace::OpenAction::Delete,
            LocalClId::DEFAULT,
        )
        .await;
    assert!(err.is_ok(), "opening is local");
    let err = ws_b
        .submit(
            &bob_conn,
            &bob.cache(),
            LocalClId::DEFAULT,
            Some("nope".into()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
}

#[tokio::test]
async fn service_accounts_use_issued_tokens() {
    let server = TestServer::start().await;
    server
        .admin(Request::CreateUser {
            name: "ci".into(),
            password: None,
            admin: false,
            replica: false,
        })
        .await;
    let anonymous = server.anonymous().await;
    let login = Request::Login {
        user: "ci".into(),
        password: PASSWORD.into(),
        code: None,
    };
    assert!(
        anonymous.call(&login).await.is_err(),
        "service accounts cannot log in with a password"
    );

    let Reply::Token { token, .. } = server
        .admin(Request::IssueToken {
            user: "ci".into(),
            ttl_days: 7,
        })
        .await
    else {
        panic!("expected a token");
    };
    let trust = Trust::Pinned(server.fingerprint().to_owned());
    let ci = Connection::connect(server.addr(), trust.clone(), Auth::Bearer(token.clone()))
        .await
        .unwrap();
    assert_eq!(ci.welcome().user.as_deref(), Some("ci"));
    ci.call(&Request::ListStreams).await.unwrap();

    server.clock().advance(8 * 24 * 3600 * 1000);
    assert!(
        Connection::connect(server.addr(), trust, Auth::Bearer(token))
            .await
            .is_err()
    );
}
