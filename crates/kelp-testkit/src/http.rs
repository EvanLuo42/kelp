//! A minimal HTTP server that records the requests it receives.
//!
//! Enabled by the `net` feature.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A request received by a [`Receiver`].
#[derive(Clone, Debug)]
pub struct Received {
    pub method: String,
    pub path: String,
    /// Header names in lower case, with their values.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    /// Returns the value of header `name`, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Answers a request with a status and a body.
pub type Handler = Arc<dyn Fn(&Received) -> (u16, Vec<u8>) + Send + Sync>;

#[derive(Default)]
struct Shared {
    received: Vec<Received>,
    /// Statuses to answer the next requests with before `handler`.
    statuses: VecDeque<u16>,
    handler: Option<Handler>,
}

/// An HTTP/1.1 server on a loopback port. It answers with an empty `200`
/// unless told otherwise, and stops when dropped.
pub struct Receiver {
    base: String,
    url: String,
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl Receiver {
    /// Starts the server.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    pub async fn start() -> Self {
        Self::start_with(None).await
    }

    /// Starts a server that answers with `handler`, if given.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    pub async fn start_with(handler: Option<Handler>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind receiver");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("receiver address")
        );
        let url = format!("{base}/hook");
        let shared = Arc::new(Mutex::new(Shared {
            handler,
            ..Shared::default()
        }));
        let accepting = shared.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, accepting.clone()));
            }
        });
        Self {
            base,
            url,
            shared,
            task,
        }
    }

    /// Returns the server's origin, such as `http://127.0.0.1:4000`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Returns the URL to send requests to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Answers the next requests with `statuses`, in order.
    pub fn respond_with(&self, statuses: &[u16]) {
        self.shared
            .lock()
            .expect("receiver lock")
            .statuses
            .extend(statuses);
    }

    /// Waits until `n` requests have been received and returns them all.
    ///
    /// # Panics
    ///
    /// Panics if they do not arrive within ten seconds.
    pub async fn wait_for(&self, n: usize) -> Vec<Received> {
        for _ in 0..1000 {
            let received = self.shared.lock().expect("receiver lock").received.clone();
            if received.len() >= n {
                return received;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} requests");
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(mut stream: TcpStream, shared: Arc<Mutex<Shared>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        start.next().unwrap_or_default().to_owned(),
        start.next().unwrap_or_default().to_owned(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = buf[head_end..head_end + length].to_vec();
    let (status, body) = {
        let mut shared = shared.lock().expect("receiver lock");
        let received = Received {
            method,
            path,
            headers,
            body,
        };
        let answer = match (shared.statuses.pop_front(), &shared.handler) {
            (Some(status), _) => (status, Vec::new()),
            (None, Some(handler)) => handler(&received),
            (None, None) => (200, Vec::new()),
        };
        shared.received.push(received);
        answer
    };
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
}

/// A fake OpenID Connect provider for the device authorization grant.
///
/// Logins stay pending until [`MockIdp::approve`] is called; token requests
/// then get an ID token with the given claims. Tokens are not signed, which
/// Kelp does not need because it gets them directly from the token endpoint.
pub struct MockIdp {
    receiver: Receiver,
    approved: Arc<Mutex<Option<serde_json::Value>>>,
}

impl MockIdp {
    /// Starts the provider.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    pub async fn start() -> Self {
        let approved: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let issuer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let (claims, origin) = (approved.clone(), issuer.clone());
        let handler: Handler = Arc::new(move |request| {
            let issuer = origin.lock().expect("idp lock").clone();
            let json = |status, value: serde_json::Value| (status, value.to_string().into_bytes());
            match request.path.as_str() {
                "/.well-known/openid-configuration" => json(
                    200,
                    serde_json::json!({
                        "issuer": issuer,
                        "device_authorization_endpoint": format!("{issuer}/device"),
                        "token_endpoint": format!("{issuer}/token"),
                    }),
                ),
                "/device" => json(
                    200,
                    serde_json::json!({
                        "device_code": "device-1",
                        "user_code": "WDJB-MJHT",
                        "verification_uri": format!("{issuer}/activate"),
                        "interval": 1,
                        "expires_in": 600,
                    }),
                ),
                "/token" => match claims.lock().expect("idp lock").clone() {
                    Some(claims) => json(
                        200,
                        serde_json::json!({ "id_token": jwt(&claims), "token_type": "Bearer" }),
                    ),
                    None => json(400, serde_json::json!({ "error": "authorization_pending" })),
                },
                _ => (404, Vec::new()),
            }
        });
        let receiver = Receiver::start_with(Some(handler)).await;
        *issuer.lock().expect("idp lock") = receiver.base().to_owned();
        Self { receiver, approved }
    }

    /// Returns the issuer URL.
    pub fn issuer(&self) -> &str {
        self.receiver.base()
    }

    /// Approves pending logins with an ID token holding `claims`.
    pub fn approve(&self, claims: serde_json::Value) {
        *self.approved.lock().expect("idp lock") = Some(claims);
    }
}

fn jwt(claims: &serde_json::Value) -> String {
    use base64::Engine;
    let encode = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    format!(
        "{}.{}.unsigned",
        encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        encode(claims.to_string().as_bytes())
    )
}
