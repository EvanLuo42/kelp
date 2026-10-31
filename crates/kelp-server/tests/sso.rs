use kelp_client::Error;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_server::{OidcConfig, totp_code};
use kelp_testkit::http::MockIdp;
use kelp_testkit::net::{ADMIN, PASSWORD, TestServer};
use serde_json::json;

fn kind(result: Result<Reply, Error>) -> ErrorKind {
    match result {
        Err(Error::Remote(e)) => e.kind,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn login(user: &str, code: Option<String>) -> Request {
    Request::Login {
        user: user.into(),
        password: PASSWORD.into(),
        code,
    }
}

#[tokio::test]
async fn totp_is_required_once_enabled_and_codes_are_not_reusable() {
    let server = TestServer::start().await;
    let alice = server.add_user("alice").await;
    let now = || kelp_core::time::Clock::now(server.clock());
    let Reply::TotpSecret { secret, uri } = alice.call(&Request::TotpEnroll).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert!(uri.starts_with("otpauth://totp/Kelp:alice?secret="));
    let wrong = Request::TotpConfirm {
        secret: secret.clone(),
        code: "000000".into(),
    };
    assert_eq!(kind(alice.call(&wrong).await), ErrorKind::Invalid);
    let confirm = Request::TotpConfirm {
        secret: secret.clone(),
        code: totp_code(&secret, now()),
    };
    alice.call(&confirm).await.unwrap();

    let conn = server.anonymous().await;
    assert_eq!(
        kind(conn.call(&login("alice", None)).await),
        ErrorKind::CodeRequired
    );
    let code = totp_code(&secret, now());
    conn.call(&login("alice", Some(code.clone())))
        .await
        .unwrap();
    assert_eq!(
        kind(conn.call(&login("alice", Some(code))).await),
        ErrorKind::Unauthenticated,
        "a code works once"
    );
    server.clock().advance(30_000);
    conn.call(&login("alice", Some(totp_code(&secret, now()))))
        .await
        .unwrap();

    let disable = Request::TotpDisable {
        user: None,
        code: None,
    };
    assert_eq!(kind(alice.call(&disable).await), ErrorKind::Forbidden);
    server.clock().advance(30_000);
    alice
        .call(&Request::TotpDisable {
            user: None,
            code: Some(totp_code(&secret, now())),
        })
        .await
        .unwrap();
    conn.call(&login("alice", None)).await.unwrap();
}

#[tokio::test]
async fn single_sign_on_creates_users_and_maps_groups() {
    let idp = MockIdp::start().await;
    let config = OidcConfig {
        issuer: idp.issuer().to_owned(),
        client_id: "kelp".into(),
        client_secret: None,
        scopes: "openid profile".into(),
        username_claim: "preferred_username".into(),
        groups_claim: Some("groups".into()),
        auto_create: true,
        require: false,
        managed_groups: Vec::new(),
    };
    let server = TestServer::start_configured(|c| c.oidc = Some(config)).await;
    server
        .admin(Request::SetGroup {
            name: "artists".into(),
            members: vec![ADMIN.into()],
        })
        .await;
    let exp = kelp_core::time::Clock::now(server.clock()) / 1000 + 3600;

    let conn = server.anonymous().await;
    let Reply::DeviceCode(device) = conn.call(&Request::OidcStart).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(device.user_code, "WDJB-MJHT");
    let poll = Request::OidcPoll {
        device_code: device.device_code.clone(),
    };
    assert_eq!(conn.call(&poll).await.unwrap(), Reply::AuthPending);

    idp.approve(
        json!({ "iss": idp.issuer(), "aud": "other", "exp": exp, "sub": "x", "preferred_username": "x" }),
    );
    assert_eq!(kind(conn.call(&poll).await), ErrorKind::Unauthenticated);

    idp.approve(json!({
        "iss": idp.issuer(),
        "aud": ["kelp"],
        "exp": exp,
        "sub": "carol-subject",
        "preferred_username": "carol@studio",
        "groups": ["artists", "unknown"],
    }));
    let Reply::Token { user, .. } = conn.call(&poll).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(user, "carol_studio");
    let Reply::Groups(groups) = conn.call(&Request::ListGroups).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert_eq!(groups[0].members, [ADMIN, "carol_studio"]);

    let edge = server.start_edge("edge").await;
    let via_edge = edge.anonymous().await;
    let Reply::DeviceCode(device) = via_edge.call(&Request::OidcStart).await.unwrap() else {
        panic!("unexpected reply");
    };
    let Reply::Token { user, .. } = via_edge
        .call(&Request::OidcPoll {
            device_code: device.device_code,
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(user, "carol_studio");
    via_edge.call(&Request::ListStreams).await.unwrap();
}

fn config(idp: &MockIdp) -> OidcConfig {
    OidcConfig {
        issuer: idp.issuer().to_owned(),
        client_id: "kelp".into(),
        client_secret: None,
        scopes: "openid profile".into(),
        username_claim: "preferred_username".into(),
        groups_claim: Some("groups".into()),
        auto_create: true,
        require: false,
        managed_groups: Vec::new(),
    }
}

/// Signs on through `conn` as the provider user `subject` named `name`,
/// in `groups`; with `link`, links that identity to the user `conn` is
/// logged in as.
async fn sign_on(
    server: &TestServer,
    idp: &MockIdp,
    conn: &kelp_client::Connection,
    (subject, name, groups): (&str, &str, &[&str]),
    link: bool,
) -> Result<Reply, Error> {
    let Reply::DeviceCode(device) = conn.call(&Request::OidcStart).await.unwrap() else {
        panic!("unexpected reply");
    };
    let exp = kelp_core::time::Clock::now(server.clock()) / 1000 + 3600;
    idp.approve(json!({
        "iss": idp.issuer(),
        "aud": "kelp",
        "exp": exp,
        "sub": subject,
        "preferred_username": name,
        "groups": groups,
    }));
    let request = match link {
        true => Request::OidcLink {
            device_code: device.device_code,
        },
        false => Request::OidcPoll {
            device_code: device.device_code,
        },
    };
    conn.call(&request).await
}

fn signed_on_as(reply: Result<Reply, Error>) -> String {
    match reply {
        Ok(Reply::Token { user, .. }) => user,
        other => panic!("expected a token, got {other:?}"),
    }
}

#[tokio::test]
async fn single_sign_on_never_takes_over_an_existing_user() {
    let idp = MockIdp::start().await;
    let server = TestServer::start_configured(|c| c.oidc = Some(config(&idp))).await;
    server.add_user("alice").await;

    let anon = server.anonymous().await;
    let err = sign_on(&server, &idp, &anon, ("evil", ADMIN, &[]), false).await;
    assert_eq!(
        kind(err),
        ErrorKind::Unauthenticated,
        "not even the administrator"
    );
    let err = sign_on(&server, &idp, &anon, ("a-1", "alice", &[]), false).await;
    assert_eq!(kind(err), ErrorKind::Unauthenticated);

    let alice = server.login("alice").await;
    let linked = sign_on(&server, &idp, &alice, ("a-1", "alice", &[]), true).await;
    assert_eq!(signed_on_as(linked), "alice");
    assert_eq!(
        kind(server.anonymous().await.call(&login("alice", None)).await),
        ErrorKind::Unauthenticated,
        "the password no longer works"
    );
    let renamed = sign_on(&server, &idp, &anon, ("a-1", "alice.new", &[]), false).await;
    assert_eq!(
        signed_on_as(renamed),
        "alice",
        "the subject decides, not the name"
    );
}

#[tokio::test]
async fn administrators_link_users_whose_names_differ() {
    let idp = MockIdp::start().await;
    let server = TestServer::start_configured(|c| c.oidc = Some(config(&idp))).await;
    server.add_user("evan").await;
    server
        .admin(Request::ExpectSso {
            user: "evan".into(),
            sso_name: Some("alice.smith".into()),
        })
        .await;
    let anon = server.anonymous().await;
    let reply = sign_on(&server, &idp, &anon, ("z-1", "alice.smith", &[]), false).await;
    assert_eq!(signed_on_as(reply), "evan");
    let Reply::User(info) = server
        .admin(Request::GetUser {
            name: "evan".into(),
        })
        .await
    else {
        panic!("unexpected reply");
    };
    assert_eq!(
        info.sso.as_deref(),
        Some(format!("z-1@{}", idp.issuer()).as_str())
    );
    assert!(!info.password);

    server
        .admin(Request::MergeUser {
            from: "evan".into(),
            into: "alice.smith".into(),
        })
        .await;
    let reply = sign_on(&server, &idp, &anon, ("z-1", "alice.smith", &[]), false).await;
    assert_eq!(signed_on_as(reply), "alice.smith");
}

#[tokio::test]
async fn single_sign_on_can_be_required_and_manage_groups() {
    let idp = MockIdp::start().await;
    let mut config = config(&idp);
    config.require = true;
    config.managed_groups = vec!["artists".into()];
    let server = TestServer::start_configured(|c| c.oidc = Some(config)).await;
    for group in ["artists", "misc"] {
        server
            .admin(Request::SetGroup {
                name: group.into(),
                members: vec![ADMIN.into()],
            })
            .await;
    }
    server
        .admin(Request::CreateUser {
            name: "bob".into(),
            password: Some(PASSWORD.into()),
            admin: false,
            replica: false,
        })
        .await;
    let anon = server.anonymous().await;
    assert_eq!(
        kind(anon.call(&login("bob", None)).await),
        ErrorKind::Unauthenticated
    );
    anon.call(&login(ADMIN, None))
        .await
        .expect("administrators keep their password");

    let members = |groups: Vec<kelp_proto::rpc::GroupInfo>| -> Vec<(String, bool)> {
        groups
            .into_iter()
            .map(|g| (g.name, g.members.iter().any(|m| m == "carol")))
            .collect()
    };
    sign_on(
        &server,
        &idp,
        &anon,
        ("c-1", "carol", &["artists", "misc"]),
        false,
    )
    .await
    .unwrap();
    let Reply::Groups(groups) = server.admin(Request::ListGroups).await else {
        panic!("unexpected reply");
    };
    assert_eq!(
        members(groups),
        [("artists".to_owned(), true), ("misc".to_owned(), true)]
    );
    sign_on(&server, &idp, &anon, ("c-1", "carol", &[]), false)
        .await
        .unwrap();
    let Reply::Groups(groups) = server.admin(Request::ListGroups).await else {
        panic!("unexpected reply");
    };
    assert_eq!(
        members(groups),
        [("artists".to_owned(), false), ("misc".to_owned(), true)],
        "only managed groups lose members"
    );
}

#[tokio::test]
async fn a_renamed_user_logs_in_under_either_name() {
    let server = TestServer::start().await;
    server.add_user("evan").await;
    server
        .admin(Request::MergeUser {
            from: "evan".into(),
            into: "alice".into(),
        })
        .await;
    let anon = server.anonymous().await;
    assert_eq!(signed_on_as(anon.call(&login("evan", None)).await), "alice");
    let reuse = server
        .login(ADMIN)
        .await
        .call(&Request::CreateUser {
            name: "evan".into(),
            password: Some(PASSWORD.into()),
            admin: false,
            replica: false,
        })
        .await;
    assert_eq!(kind(reuse), ErrorKind::AlreadyExists);
}
