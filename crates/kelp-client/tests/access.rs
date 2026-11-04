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

/// Submits `files` to `//game/main` as `conn`'s user from a new workspace.
async fn submit_files(server: &TestServer, conn: &Connection, files: &[(&str, &str)]) {
    let client = TestClient::new();
    let mut ws = client.create_workspace(server, "ws", &depot_path("//game/main"));
    for (path, text) in files {
        client.sandbox().write(&format!("ws/{path}"), text);
    }
    ws.reconcile(conn).await.unwrap();
    ws.submit(
        conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("files".into()),
    )
    .await
    .unwrap();
}

async fn manifest_of(conn: &Connection, text: &str) -> Option<kelp_core::FileManifest> {
    let request = Request::GetManifests {
        ids: vec![kelp_core::ContentId::of(text.as_bytes())],
    };
    let Reply::Manifests(mut found) = conn.call(&request).await.unwrap() else {
        panic!("expected manifests");
    };
    found.pop().unwrap()
}

#[tokio::test]
async fn content_is_read_only_through_paths_the_user_can_read() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let (alice, bob) = (server.add_user("alice").await, server.add_user("bob").await);
    let table = "write * //...\nnone user:bob //game/main/Secret/...\nnone user:bob //game/main/Early/...\nread user:bob //game/dev/...\nnone user:bob //game/dev/Secret/...\n";
    server
        .admin(Request::SetProtections {
            source: table.into(),
        })
        .await;
    submit_files(
        &server,
        &alice,
        &[
            ("Secret/plan.txt", "the plan"),
            ("Art/tree.txt", "a tree"),
            ("Early/x.txt", "inherited"),
        ],
    )
    .await;
    server
        .admin(Request::CreateStream {
            path: "//game/dev".into(),
            kind: kelp_proto::rpc::StreamKind::Development,
            parent: Some("//game/main".into()),
        })
        .await;

    let plan = manifest_of(&alice, "the plan")
        .await
        .expect("alice reads it");
    assert_eq!(
        manifest_of(&bob, "the plan").await,
        None,
        "held back like missing content"
    );
    let tree = manifest_of(&bob, "a tree")
        .await
        .expect("bob reads the tree");
    assert!(
        manifest_of(&bob, "inherited").await.is_some(),
        "dev started with it, and bob reads dev"
    );

    let chunk = |m: &kelp_core::FileManifest| (m.chunks[0].id, m.content);
    let err = bob.get_chunks(&[plan.chunks[0].id]).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
    assert!(bob.get_content_chunks(&[chunk(&plan)]).await.is_err());
    let lying = [(plan.chunks[0].id, tree.content)];
    let err = bob.get_content_chunks(&lying).await.unwrap_err();
    assert!(
        matches!(err, Error::Remote(ref e) if e.kind == ErrorKind::Forbidden),
        "{err}"
    );
    assert_eq!(
        bob.get_content_chunks(&[chunk(&tree)]).await.unwrap().len(),
        1
    );
    assert_eq!(
        alice.get_chunks(&[plan.chunks[0].id]).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn shelves_are_listed_newest_first_for_streams_one_can_see() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server.create_depot("secret").await;
    let (alice, bob) = (server.add_user("alice").await, server.add_user("bob").await);
    server
        .admin(Request::SetProtections {
            source: "write * //...\nnone user:bob //secret/...\n".into(),
        })
        .await;
    let client = TestClient::new();
    for (stream, file) in [
        ("//game/main", "a.txt"),
        ("//secret/main", "s.txt"),
        ("//game/main", "b.txt"),
    ] {
        let name = format!(
            "ws{}",
            file.len() + stream.len() + usize::from(file == "b.txt")
        );
        let mut ws = client.create_workspace(&server, &name, &depot_path(stream));
        client.sandbox().write(&format!("{name}/{file}"), "x");
        ws.reconcile(&alice).await.unwrap();
        ws.shelve(&alice, &client.cache(), LocalClId::DEFAULT)
            .await
            .unwrap();
    }
    let list = |conn: Connection, stream: Option<&str>, owner: Option<&str>, limit| {
        let request = Request::ListShelves {
            stream: stream.map(str::to_owned),
            owner: owner.map(str::to_owned),
            limit,
        };
        async move {
            let Reply::Shelves(s) = conn.call(&request).await.unwrap() else {
                panic!("expected shelves");
            };
            s.into_iter()
                .map(|s| (s.stream, s.files))
                .collect::<Vec<_>>()
        }
    };
    let game = ("//game/main".to_owned(), 1);
    let secret = ("//secret/main".to_owned(), 1);
    assert_eq!(
        list(alice.clone(), None, None, 0).await,
        [game.clone(), secret, game.clone()]
    );
    assert_eq!(
        list(bob.clone(), None, None, 0).await,
        [game.clone(), game.clone()]
    );
    assert_eq!(
        list(bob.clone(), None, None, 1).await,
        std::slice::from_ref(&game)
    );
    assert_eq!(list(bob.clone(), None, Some("bob"), 0).await, []);
    assert_eq!(
        list(alice, Some("//game/main"), Some("ALICE"), 0).await,
        [game.clone(), game]
    );
}

#[tokio::test]
async fn depot_administrators_and_roles_hold_part_of_the_powers() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    server.create_depot("engine").await;
    let (lead, it) = (server.add_user("lead").await, server.add_user("it").await);
    let table = "write * //...\nadmin user:lead //game/...\nrole accounts user:it\n";
    server
        .admin(Request::SetProtections {
            source: table.into(),
        })
        .await;
    let forbidden = |r: Result<Reply, Error>| {
        assert!(
            matches!(r, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden),
            "{r:?}"
        );
    };
    let dev = |depot: &str| Request::CreateStream {
        path: format!("//{depot}/dev"),
        kind: kelp_proto::rpc::StreamKind::Development,
        parent: Some(format!("//{depot}/main")),
    };
    lead.call(&dev("game")).await.unwrap();
    forbidden(lead.call(&dev("engine")).await);
    forbidden(it.call(&dev("game")).await);
    forbidden(
        lead.call(&Request::CreateDepot {
            name: "tools".into(),
        })
        .await,
    );
    lead.call(&Request::SetTypemap {
        depot: "game".into(),
        source: String::new(),
    })
    .await
    .unwrap();
    forbidden(lead.call(&Request::ListUsers).await);
    forbidden(lead.call(&Request::Gc { dry_run: true }).await);

    it.call(&Request::ListUsers).await.unwrap();
    let user = |name: &str, admin| Request::CreateUser {
        name: name.into(),
        password: Some(PASSWORD.into()),
        admin,
        replica: false,
    };
    it.call(&user("carol", false)).await.unwrap();
    forbidden(it.call(&user("root2", true)).await);
    let reset = |name: &str| Request::SetPassword {
        user: Some(name.into()),
        current: None,
        password: "a new password".into(),
    };
    it.call(&reset("carol")).await.unwrap();
    forbidden(it.call(&reset(kelp_testkit::net::ADMIN)).await);
    forbidden(it.call(&Request::GetProtections).await);
    forbidden(
        it.call(&Request::SetProtections {
            source: "write * //...\n".into(),
        })
        .await,
    );
}
