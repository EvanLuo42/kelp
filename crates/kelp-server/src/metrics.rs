//! A plain HTTP endpoint for monitoring.
//!
//! `GET /healthz` answers `200 ok` while the server can serve, and `503`
//! with the reason otherwise: an edge whose replication stopped, or a commit
//! group member that sees no leader. `GET /metrics` answers in the
//! Prometheus text format.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::State;
use crate::role::Role;

pub(crate) async fn serve(state: Arc<State>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = answer(&state, stream).await;
        });
    }
}

async fn answer(state: &State, mut stream: TcpStream) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 8192 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&request);
    let path = line.split_whitespace().nth(1).unwrap_or("");
    let (status, body) = match path {
        "/healthz" => match health(state) {
            Ok(()) => ("200 OK", "ok\n".to_owned()),
            Err(why) => ("503 Service Unavailable", format!("{why}\n")),
        },
        "/metrics" => ("200 OK", metrics(state)),
        _ => ("404 Not Found", "not found\n".to_owned()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn health(state: &State) -> Result<(), String> {
    state.reader().map_err(|e| e.message)?;
    match &state.role {
        Role::Edge(edge) => edge.check().map_err(|e| e.message),
        Role::Member(_) => match state.cluster() {
            Some(cluster) if cluster.status().leader.is_some() => Ok(()),
            Some(_) => Err("the commit group has no leader".into()),
            None => Err("the commit group is starting".into()),
        },
        Role::AllInOne => Ok(()),
    }
}

fn metrics(state: &State) -> String {
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, value: u64| {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
        );
    };
    gauge(
        "kelp_up",
        "Whether the server can serve.",
        u64::from(health(state).is_ok()),
    );
    if let Ok(reader) = state.reader() {
        if let Ok(index) = reader.last_index() {
            gauge(
                "kelp_log_index",
                "The index of the last command in the log.",
                index.0,
            );
        }
        if let Ok(streams) = reader.streams() {
            gauge(
                "kelp_streams",
                "The number of streams.",
                streams.len() as u64,
            );
        }
        if let Ok(users) = reader.users() {
            gauge("kelp_users", "The number of users.", users.len() as u64);
        }
    }
    match &state.role {
        Role::Edge(edge) => gauge(
            "kelp_edge_applied_index",
            "The last log entry this edge has replicated.",
            edge.applied(),
        ),
        Role::Member(_) => {
            if let Some(cluster) = state.cluster() {
                let status = cluster.status();
                gauge(
                    "kelp_cluster_leader",
                    "Whether this member leads its commit group.",
                    u64::from(status.leader == Some(status.member)),
                );
                gauge(
                    "kelp_cluster_applied_index",
                    "The last Raft log entry this member has applied.",
                    status.applied.unwrap_or(0),
                );
            }
        }
        Role::AllInOne => {}
    }
    let mut counter = |name: &str, help: &str, value: u64| {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
        );
    };
    counter(
        "kelp_requests_total",
        "Requests handled since the server started.",
        state.requests.load(Ordering::Relaxed),
    );
    counter(
        "kelp_request_errors_total",
        "Requests that failed since the server started.",
        state.failures.load(Ordering::Relaxed),
    );
    out
}
