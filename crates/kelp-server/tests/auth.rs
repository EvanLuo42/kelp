use kelp_client::{Connection, Error, Trust};
use kelp_proto::{Auth, ErrorKind, Reply, Request};
use kelp_testkit::net::{ADMIN, PASSWORD, TestServer};

fn remote_kind(err: Error) -> ErrorKind {
    match err {
        Error::Remote(e) => e.kind,
        other => panic!("expected a server error, got {other}"),
    }
}

#[tokio::test]
async fn requests_need_a_login() {
    let server = TestServer::start().await;
    let conn = server.anonymous().await;
    let err = conn.call(&Request::ListStreams).await.unwrap_err();
    assert_eq!(remote_kind(err), ErrorKind::Unauthenticated);

    let wrong = Request::Login {
        user: ADMIN.into(),
        password: "nope".into(),
        code: None,
    };
    assert_eq!(
        remote_kind(conn.call(&wrong).await.unwrap_err()),
        ErrorKind::Unauthenticated
    );
}

#[tokio::test]
async fn only_admins_manage_users_and_depots() {
    let server = TestServer::start().await;
    let alice = server.add_user("alice").await;
    let create = Request::CreateDepot {
        name: "game".into(),
    };
    assert_eq!(
        remote_kind(alice.call(&create).await.unwrap_err()),
        ErrorKind::Forbidden
    );

    let admin = server.login(ADMIN).await;
    admin.call(&create).await.unwrap();
    assert_eq!(
        remote_kind(admin.call(&create).await.unwrap_err()),
        ErrorKind::AlreadyExists
    );
    let Reply::Streams(streams) = alice.call(&Request::ListStreams).await.unwrap() else {
        panic!("expected streams");
    };
    let paths: Vec<_> = streams.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(paths, ["//game/main"]);

    let dup = Request::CreateUser {
        name: "ALICE".into(),
        password: Some(PASSWORD.into()),
        admin: false,
        replica: false,
    };
    assert_eq!(
        remote_kind(admin.call(&dup).await.unwrap_err()),
        ErrorKind::AlreadyExists
    );
}

#[tokio::test]
async fn tokens_authenticate_new_connections_until_they_expire() {
    let server = TestServer::start().await;
    let conn = server.anonymous().await;
    let login = Request::Login {
        user: ADMIN.into(),
        password: PASSWORD.into(),
        code: None,
    };
    let Reply::Token { token, .. } = conn.call(&login).await.unwrap() else {
        panic!("expected token");
    };

    let trust = Trust::Pinned(server.fingerprint().to_owned());
    let again = Connection::connect(server.addr(), trust.clone(), Auth::Bearer(token.clone()))
        .await
        .unwrap();
    assert_eq!(again.welcome().user.as_deref(), Some(ADMIN));

    server.clock().advance(31 * 24 * 3600 * 1000);
    let expired = Connection::connect(server.addr(), trust, Auth::Bearer(token)).await;
    assert!(matches!(expired, Err(Error::Remote(e)) if e.kind == ErrorKind::Unauthenticated));
}

#[tokio::test]
async fn clients_refuse_an_unexpected_certificate() {
    let server = TestServer::start().await;
    let first = Connection::connect(server.addr(), Trust::FirstUse, Auth::None)
        .await
        .unwrap();
    assert_eq!(first.fingerprint(), server.fingerprint());

    let wrong = Trust::Pinned("0".repeat(64));
    let err = Connection::connect(server.addr(), wrong, Auth::None)
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, Error::UntrustedServer { .. }), "{err}");
}

fn login_as(user: &str, password: &str) -> Request {
    Request::Login {
        user: user.into(),
        password: password.into(),
        code: None,
    }
}

#[tokio::test]
async fn users_change_their_password_and_administrators_reset_others() {
    let server = TestServer::start().await;
    let alice = server.add_user("alice").await;
    server.add_user("bob").await;
    let change = |current: &str, password: &str| Request::SetPassword {
        user: None,
        current: Some(current.into()),
        password: password.into(),
    };
    assert!(matches!(
        alice.call(&change("wrong", "new password")).await,
        Err(Error::Remote(ref e)) if e.kind == ErrorKind::Unauthenticated
    ));
    alice.call(&change(PASSWORD, "new password")).await.unwrap();
    let anon = server.anonymous().await;
    assert!(anon.call(&login_as("alice", PASSWORD)).await.is_err());
    anon.call(&login_as("alice", "new password")).await.unwrap();

    let reset_bob = Request::SetPassword {
        user: Some("bob".into()),
        current: None,
        password: "reset by admin".into(),
    };
    assert!(matches!(
        alice.call(&reset_bob).await,
        Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden
    ));
    server.admin(reset_bob).await;
    anon.call(&login_as("bob", "reset by admin")).await.unwrap();
}

#[tokio::test]
async fn a_disabled_user_is_shut_out_until_enabled() {
    let server = TestServer::start().await;
    let alice = server.add_user("alice").await;
    alice.call(&Request::ListStreams).await.unwrap();
    server
        .admin(Request::SetDisabled {
            user: "alice".into(),
            disabled: true,
        })
        .await;
    assert!(
        matches!(
            alice.call(&Request::ListStreams).await,
            Err(Error::Remote(ref e)) if e.kind == ErrorKind::Unauthenticated
        ),
        "an open session stops working"
    );
    let anon = server.anonymous().await;
    assert!(anon.call(&login_as("alice", PASSWORD)).await.is_err());
    let Reply::Users(users) = server.admin(Request::ListUsers).await else {
        panic!("unexpected reply");
    };
    assert!(users.iter().any(|u| u.name == "alice" && u.disabled));

    server
        .admin(Request::SetDisabled {
            user: "alice".into(),
            disabled: false,
        })
        .await;
    anon.call(&login_as("alice", PASSWORD)).await.unwrap();
}

#[tokio::test]
async fn logging_out_revokes_the_token() {
    let server = TestServer::start().await;
    server.add_user("alice").await;
    let anon = server.anonymous().await;
    let Reply::Token { token, .. } = anon.call(&login_as("alice", PASSWORD)).await.unwrap() else {
        panic!("unexpected reply");
    };
    let connect = || {
        Connection::connect(
            server.addr(),
            Trust::Pinned(server.fingerprint().to_owned()),
            Auth::Bearer(token.clone()),
        )
    };
    let conn = connect().await.unwrap();
    conn.call(&Request::Logout).await.unwrap();
    let revoked = connect().await;
    assert!(matches!(revoked, Err(Error::Remote(e)) if e.kind == ErrorKind::Unauthenticated));
}

#[tokio::test]
async fn revoking_a_token_ends_the_sessions_using_it() {
    let server = TestServer::start().await;
    server.add_user("alice").await;
    let alice = server.anonymous().await;
    let Reply::Token { token, .. } = alice.call(&login_as("alice", PASSWORD)).await.unwrap() else {
        panic!("unexpected reply");
    };
    let elsewhere = Connection::connect(
        server.addr(),
        Trust::Pinned(server.fingerprint().to_owned()),
        Auth::Bearer(token),
    )
    .await
    .unwrap();
    alice.call(&Request::ListStreams).await.unwrap();
    elsewhere.call(&Request::ListStreams).await.unwrap();

    elsewhere.call(&Request::Logout).await.unwrap();
    for conn in [&alice, &elsewhere] {
        let err = conn.call(&Request::ListStreams).await.unwrap_err();
        assert_eq!(remote_kind(err), ErrorKind::Unauthenticated);
    }

    // A session that logged in with a password can log out too.
    let bob = server.add_user("bob").await;
    bob.call(&Request::Logout).await.unwrap();
    let err = bob.call(&Request::ListStreams).await.unwrap_err();
    assert_eq!(remote_kind(err), ErrorKind::Unauthenticated);
}

#[tokio::test]
async fn failed_logins_do_not_tell_which_users_exist() {
    let server = TestServer::start().await;
    server.add_user("alice").await;
    server
        .admin(Request::CreateUser {
            name: "build".into(),
            password: None,
            admin: false,
            replica: false,
        })
        .await;
    let anon = server.anonymous().await;
    let mut errors = Vec::new();
    for (user, password) in [("alice", "wrong"), ("nobody", PASSWORD), ("build", PASSWORD)] {
        match anon.call(&login_as(user, password)).await {
            Err(Error::Remote(e)) => errors.push(e),
            other => panic!("expected an error, got {other:?}"),
        }
    }
    assert_eq!(errors[0].kind, ErrorKind::Unauthenticated);
    assert!(errors.iter().all(|e| *e == errors[0]), "{errors:?}");
}

#[tokio::test]
async fn a_session_follows_its_user_into_a_merge() {
    let server = TestServer::start().await;
    let alice = server.add_user("alice").await;
    server.add_user("bob").await;
    server
        .admin(Request::MergeUser {
            from: "alice".into(),
            into: "bob".into(),
        })
        .await;
    alice
        .call(&Request::EditProfile {
            user: None,
            change: kelp_proto::rpc::ProfileChange::DisplayName(Some("Bob B".into())),
        })
        .await
        .unwrap();
    let Reply::User(bob) = server.admin(Request::GetUser { name: "bob".into() }).await else {
        panic!("unexpected reply");
    };
    assert_eq!(bob.display_name.as_deref(), Some("Bob B"));
}
