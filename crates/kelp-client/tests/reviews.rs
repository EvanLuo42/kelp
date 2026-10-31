use kelp_client::Error;
use kelp_core::LocalClId;
use kelp_proto::rpc::{CheckState, ReviewState};
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};

#[tokio::test]
async fn a_covered_change_is_submitted_through_an_approved_review() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let alice_conn = server.add_user("alice").await;
    let bob_conn = server.add_user("bob").await;
    let ci_conn = server.add_user("ci").await;
    server
        .admin(Request::SetGroup {
            name: "leads".into(),
            members: vec!["bob".into()],
        })
        .await;
    let rules = "approve 1 group:leads //game/main/Code/...\nstatus build //game/...\n";
    let forbidden = alice_conn
        .call(&Request::SetReviewRules {
            source: rules.into(),
        })
        .await;
    assert!(matches!(forbidden, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden));
    server
        .admin(Request::SetReviewRules {
            source: rules.into(),
        })
        .await;

    let alice = TestClient::new();
    let mut ws = alice.create_workspace(&server, "ws", &depot_path("//game/main"));
    alice
        .sandbox()
        .write("ws/Code/render.rs", "fn render() {}\n");
    ws.reconcile(&alice_conn).await.unwrap();
    let blocked = ws
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("render".into()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(blocked, Error::Remote(ref e) if e.kind == ErrorKind::Conflict && e.message.contains("needs a review")),
        "{blocked}"
    );

    let shelf = ws
        .shelve(&alice_conn, &alice.cache(), LocalClId::DEFAULT)
        .await
        .unwrap();
    alice_conn
        .call(&Request::OpenReview {
            shelf,
            reviewers: vec!["bob".into()],
        })
        .await
        .unwrap();
    bob_conn
        .call(&Request::Vote {
            shelf,
            version: 1,
            approve: Some(true),
        })
        .await
        .unwrap();
    let Reply::Review(info) = bob_conn.call(&Request::GetReview { shelf }).await.unwrap() else {
        panic!("unexpected reply");
    };
    assert!(!info.summary.ready, "the build has not reported yet");
    assert_eq!(info.files.len(), 1);

    ci_conn
        .call(&Request::ReportStatus {
            shelf,
            version: 1,
            context: "build".into(),
            state: CheckState::Success,
            url: Some("https://ci.example/1".into()),
        })
        .await
        .unwrap();
    let cl = ws
        .submit(
            &alice_conn,
            &alice.cache(),
            LocalClId::DEFAULT,
            Some("render".into()),
        )
        .await
        .unwrap();

    let Reply::Reviews(reviews) = bob_conn
        .call(&Request::ListReviews {
            stream: None,
            open_only: false,
        })
        .await
        .unwrap()
    else {
        panic!("unexpected reply");
    };
    assert_eq!(reviews[0].state, ReviewState::Submitted(cl));
    let open = bob_conn
        .call(&Request::ListReviews {
            stream: Some("//game/main".into()),
            open_only: true,
        })
        .await
        .unwrap();
    assert_eq!(open, Reply::Reviews(vec![]));
}
