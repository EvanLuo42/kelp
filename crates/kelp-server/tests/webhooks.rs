use kelp_client::Error;
use kelp_core::LocalClId;
use kelp_proto::{ErrorKind, Reply, Request};
use kelp_testkit::depot_path;
use kelp_testkit::http::Receiver;
use kelp_testkit::net::{TestClient, TestServer};

async fn deliveries(server: &TestServer) -> Vec<kelp_proto::rpc::DeliveryInfo> {
    match server
        .admin(Request::Deliveries {
            webhook: Some("ci".into()),
            limit: 10,
        })
        .await
    {
        Reply::Deliveries(d) => d,
        other => panic!("unexpected reply {other:?}"),
    }
}

#[tokio::test]
async fn submits_are_delivered_signed_and_retried() {
    let server = TestServer::start().await;
    server.create_depot("game").await;
    let receiver = Receiver::start().await;
    receiver.respond_with(&[500]);
    server
        .admin(Request::SetWebhook {
            name: "ci".into(),
            url: receiver.url().into(),
            secret: "s3cret".into(),
            events: vec!["submit".into()],
            paths: vec!["//game/...".into()],
        })
        .await;
    let conn = server.add_user("alice").await;
    let forbidden = conn.call(&Request::ListWebhooks).await;
    assert!(matches!(forbidden, Err(Error::Remote(ref e)) if e.kind == ErrorKind::Forbidden));

    let client = TestClient::new();
    let mut ws = client.create_workspace(&server, "ws", &depot_path("//game/main"));
    client.sandbox().write("ws/a.txt", "a");
    ws.reconcile(&conn).await.unwrap();
    let cl = ws
        .submit(
            &conn,
            &client.cache(),
            LocalClId::DEFAULT,
            Some("first".into()),
        )
        .await
        .unwrap();

    receiver.wait_for(1).await;
    let d = &deliveries(&server).await[0];
    assert_eq!((d.attempts, d.delivered, d.pending), (1, false, true));
    assert_eq!(d.last_result.as_deref(), Some("500"));

    server.clock().advance(60_000);
    let received = receiver.wait_for(2).await;
    let request = &received[1];
    assert_eq!(request.method, "POST");
    assert_eq!(request.header("x-kelp-event"), Some("submit"));
    assert_eq!(
        request.header("x-kelp-signature").map(str::to_owned),
        Some(kelp_server::signature("s3cret", &request.body))
    );
    assert_eq!(
        request.header("x-kelp-delivery"),
        received[0].header("x-kelp-delivery")
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["event"], "submit");
    assert_eq!(body["actor"], "alice");
    assert_eq!(body["data"]["cl"], cl);

    for _ in 0..100 {
        if deliveries(&server).await[0].delivered {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let d = &deliveries(&server).await[0];
    assert_eq!((d.attempts, d.delivered, d.pending), (2, true, false));

    server.admin(Request::Redeliver { id: d.id }).await;
    receiver.wait_for(3).await;
}
