use kelp_core::LocalClId;
use kelp_proto::{Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::net::{TestClient, TestServer};
use kelp_workspace::OpenAction;

const BASE: &str = "fn a() {}\n\n\n\nfn b() {}\n\n\n\nfn c() {}\n";

#[tokio::test]
async fn hunks_of_one_file_submit_separately() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let conn = server.add_user("alice").await;
    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    let path = depot_path("//game/main/lib.rs");
    client.sandbox().write("ws/lib.rs", BASE);
    ws.reconcile(&conn).await.unwrap();
    ws.submit(
        &conn,
        &client.cache(),
        LocalClId::DEFAULT,
        Some("base".into()),
    )
    .await
    .unwrap();

    let feature = ws.pending_mut().create("feature");
    let fix = ws.pending_mut().create("fix");
    ws.open_files(
        &conn,
        std::slice::from_ref(&path),
        OpenAction::Edit,
        feature,
    )
    .await
    .unwrap();
    let working = BASE
        .replace("fn a() {}", "fn a() { fixed() }")
        .replace("fn c() {}", "fn c() { new() }");
    client.sandbox().write("ws/lib.rs", &working);

    let hunks = ws.hunks(&client.cache(), &path).await.unwrap();
    assert_eq!(hunks.len(), 2);
    assert!(hunks.iter().all(|(_, owner)| *owner == feature));
    ws.pending_mut()
        .assign_hunk(&path, hunks[0].0.anchor, fix)
        .unwrap();

    let fixed_cl = ws.submit(&conn, &client.cache(), fix, None).await.unwrap();
    let Reply::Filelog(revs) = conn
        .call(&Request::Filelog {
            path: path.to_string(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(revs[0].cl, fixed_cl);
    assert_eq!(
        revs[0].content,
        Some(kelp_core::ContentId::of(
            BASE.replace("fn a() {}", "fn a() { fixed() }").as_bytes()
        ))
    );
    assert_eq!(
        ws.state().pending.action(&path),
        Some(OpenAction::Edit),
        "the rest stays opened"
    );
    assert_eq!(
        client.sandbox().read("ws/lib.rs"),
        working.as_bytes(),
        "the disk keeps all edits"
    );

    ws.submit(&conn, &client.cache(), feature, None)
        .await
        .unwrap();
    let Reply::Filelog(revs) = conn
        .call(&Request::Filelog {
            path: path.to_string(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        revs[0].content,
        Some(kelp_core::ContentId::of(working.as_bytes()))
    );
}
