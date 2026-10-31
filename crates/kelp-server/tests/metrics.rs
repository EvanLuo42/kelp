use std::io::{Read, Write};

use kelp_proto::Request;
use kelp_testkit::net::TestServer;

fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nhost: kelp\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_metrics_are_served_over_http() {
    let server = TestServer::start_configured(|c| {
        c.metrics = Some("127.0.0.1:0".parse().unwrap());
    })
    .await;
    server.create_depot("game").await;
    server.admin(Request::ListStreams).await;
    let addr = server.metrics_addr().unwrap();

    let health = tokio::task::spawn_blocking(move || get(addr, "/healthz"))
        .await
        .unwrap();
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    let metrics = tokio::task::spawn_blocking(move || get(addr, "/metrics"))
        .await
        .unwrap();
    assert!(metrics.contains("kelp_up 1"), "{metrics}");
    assert!(metrics.contains("kelp_streams 1"), "{metrics}");
    let requests: u64 = metrics
        .lines()
        .find_map(|l| l.strip_prefix("kelp_requests_total "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(requests >= 2);
    let missing = tokio::task::spawn_blocking(move || get(addr, "/nope"))
        .await
        .unwrap();
    assert!(missing.starts_with("HTTP/1.1 404"));
}
