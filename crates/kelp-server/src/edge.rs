//! The edge role: a read replica that forwards writes to the commit server.
//!
//! An edge copies the commit server's command log into its own metadata
//! database and serves reads from it. Requests that change state are sent
//! on to the commit server under the requesting user's own token, so the
//! commit server checks permissions exactly as if the user had connected
//! directly. The edge then waits until it has replicated the change before
//! replying, so a client always reads its own writes.
//!
//! File content is fetched from the commit server on first use and kept in
//! the edge's chunk store. Uploads are written through to both.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kelp_cas::BlobStore;
use kelp_client::{Connection, Trust};
use kelp_core::{ChunkId, ContentId};
use kelp_meta::LogEntry;
use kelp_proto::io::{expect_frame, split_chunk};
use kelp_proto::{Auth, ErrorKind, ErrorReply, MsgType, Reply, Request};
use quinn::RecvStream;
use tokio::sync::{Mutex, watch};

use crate::State;
use crate::role::Forwarder;

const LOG_BATCH: u32 = 1000;
const LOG_WAIT: Duration = Duration::from_secs(1);
const CATCH_UP: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How many bytes of an upload a forwarding server holds before passing
/// them on.
const FORWARD_BYTES: usize = 64 << 20;

/// Where an edge server's commit server is and how to reach it.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// The commit server's address, as `host:port`, or the addresses of
    /// several members of a commit group, which are tried in turn.
    pub addrs: Vec<String>,
    /// The fingerprint of the commit server's certificate.
    pub fingerprint: String,
    /// A token of an edge account; see `kelp user add --replica`.
    pub token: String,
}

pub(crate) struct Edge {
    upstream: Upstream,
    /// The address in `upstream.addrs` that answered last.
    current: std::sync::atomic::AtomicUsize,
    /// The edge account's connection, for replication and content.
    conn: Mutex<Option<Connection>>,
    /// The index of the last replicated log entry.
    applied: watch::Sender<u64>,
    /// Why replication stopped for good, if it did.
    failed: std::sync::Mutex<Option<String>>,
}

impl Edge {
    pub(crate) fn new(upstream: Upstream, applied: u64) -> Self {
        Self {
            upstream,
            current: Default::default(),
            conn: Mutex::new(None),
            applied: watch::Sender::new(applied),
            failed: std::sync::Mutex::new(None),
        }
    }

    /// Returns the index of the last replicated log entry.
    pub(crate) fn applied(&self) -> u64 {
        *self.applied.borrow()
    }

    /// Returns an error if replication has stopped.
    pub(crate) fn check(&self) -> Result<(), ErrorReply> {
        match &*self.failed.lock().expect("edge lock") {
            Some(why) => Err(ErrorReply::new(ErrorKind::Unavailable, why.clone())),
            None => Ok(()),
        }
    }

    /// Connects to the commit server with `auth`.
    pub(crate) async fn connect(&self, auth: Auth) -> Result<Connection, ErrorReply> {
        use std::sync::atomic::Ordering;
        let addrs = &self.upstream.addrs;
        let first = self.current.load(Ordering::Relaxed);
        let mut last = None;
        for i in (0..addrs.len()).map(|n| (first + n) % addrs.len()) {
            let trust = Trust::Pinned(self.upstream.fingerprint.clone());
            match Connection::connect(&addrs[i], trust, auth.clone()).await {
                Ok(conn) => {
                    crate::handler::check_peer_version(&conn)?;
                    self.current.store(i, Ordering::Relaxed);
                    return Ok(conn);
                }
                // Another member may answer; a rejection would be the same
                // everywhere.
                Err(kelp_client::Error::Remote(e)) => return Err(e),
                Err(e) => last = Some(e),
            }
        }
        Err(match last {
            Some(e) => upstream_error(e),
            None => ErrorReply::new(ErrorKind::Unavailable, "the edge has no upstream address"),
        })
    }

    /// Returns the edge account's connection, reconnecting if needed.
    async fn connection(&self) -> Result<Connection, ErrorReply> {
        let mut conn = self.conn.lock().await;
        if let Some(c) = conn.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let c = self
            .connect(Auth::Bearer(self.upstream.token.clone()))
            .await?;
        *conn = Some(c.clone());
        Ok(c)
    }

    /// Waits until the edge has replicated everything the commit server had
    /// committed when this was called.
    pub(crate) async fn catch_up(&self) -> Result<(), ErrorReply> {
        let request = Request::ReadLog {
            from: u64::MAX,
            limit: 0,
            wait_ms: 0,
        };
        let Reply::Log { head, .. } = self.call(&request).await? else {
            return Err(unexpected());
        };
        self.wait_for(head).await
    }

    /// Waits until log entry `index` has been replicated.
    pub(crate) async fn wait_for(&self, index: u64) -> Result<(), ErrorReply> {
        let mut applied = self.applied.subscribe();
        let wait = applied.wait_for(|&a| a >= index);
        match tokio::time::timeout(CATCH_UP, wait).await {
            Ok(Ok(_)) => Ok(()),
            _ => Err(ErrorReply::new(
                ErrorKind::Unavailable,
                format!("the edge has not replicated log entry {index} yet; try again"),
            )),
        }
    }

    async fn call(&self, request: &Request) -> Result<Reply, ErrorReply> {
        self.connection()
            .await?
            .call(request)
            .await
            .map_err(upstream_error)
    }
}

/// Copies the commit server's log into the edge's database until the edge
/// stops or its history turns out to differ from the commit server's.
pub(crate) async fn replicate(state: Arc<State>) {
    let edge = state.edge().expect("replication runs on edges");
    let mut backoff = Duration::from_millis(100);
    loop {
        match step(&state, edge).await {
            Ok(()) => backoff = Duration::from_millis(100),
            Err(Step::Fatal(why)) => {
                tracing::error!("replication stopped: {why}");
                *edge.failed.lock().expect("edge lock") = Some(why);
                return;
            }
            Err(Step::Retry(why)) => {
                tracing::warn!("replication: {why}; retrying in {backoff:?}");
                *edge.conn.lock().await = None;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

enum Step {
    Retry(String),
    Fatal(String),
}

async fn step(state: &Arc<State>, edge: &Edge) -> Result<(), Step> {
    let from = *edge.applied.borrow() + 1;
    let request = Request::ReadLog {
        from,
        limit: LOG_BATCH,
        wait_ms: LOG_WAIT.as_millis() as u32,
    };
    let (entries, head, digest) = match edge.call(&request).await {
        Ok(Reply::Log { format, .. }) if format != kelp_meta::db::FORMAT_VERSION => {
            return Err(Step::Fatal(format!(
                "the commit server has database format {format} and this edge {}; run the same Kelp version on both",
                kelp_meta::db::FORMAT_VERSION
            )));
        }
        Ok(Reply::Log {
            entries,
            head,
            digest,
            ..
        }) => (entries, head, digest),
        Ok(_) => return Err(Step::Retry("unexpected reply".into())),
        Err(e) if e.kind == ErrorKind::Forbidden || e.kind == ErrorKind::Unauthenticated => {
            return Err(Step::Fatal(format!(
                "the commit server rejected the edge account: {}",
                e.message
            )));
        }
        Err(e) => return Err(Step::Retry(e.message)),
    };
    if entries.is_empty() {
        return check_digest(state, from - 1, head, digest).await;
    }
    let last = entries.last().map(|e| e.index).unwrap_or(from - 1);
    let db_state = state.clone();
    let applied = tokio::task::spawn_blocking(move || {
        for e in entries {
            let entry = LogEntry {
                index: e.index,
                actor: e.actor,
                command: e.command,
                hash: e.hash,
            };
            db_state.db.replicate(&entry)?;
        }
        Ok::<_, kelp_meta::Error>(())
    })
    .await
    .map_err(|e| Step::Retry(e.to_string()))?;
    match applied {
        Ok(()) => {
            edge.applied.send_replace(last);
            check_digest(state, last, head, digest).await
        }
        Err(e) => Err(Step::Fatal(format!(
            "cannot apply the commit server's log: {e}"
        ))),
    }
}

/// Compares the commit server's stream digest as of `head` with this
/// edge's, if the edge has applied exactly that far. The hash chain proves
/// that both applied the same commands; this proves they got the same
/// trees from them, which could differ if, say, two builds folded case
/// differently.
async fn check_digest(
    state: &Arc<State>,
    applied: u64,
    head: u64,
    digest: kelp_core::Hash,
) -> Result<(), Step> {
    if applied != head {
        return Ok(());
    }
    let reading = state.clone();
    let ours = tokio::task::spawn_blocking(move || reading.db.read()?.stream_digest())
        .await
        .map_err(|e| Step::Retry(e.to_string()))?
        .map_err(|e| Step::Retry(e.to_string()))?;
    match ours == digest {
        true => Ok(()),
        false => Err(Step::Fatal(format!(
            "this edge's streams differ from the commit server's after log entry {head}; restore the edge from a backup of the commit server"
        ))),
    }
}

/// Sends `request` to the commit server over `conn` and waits until its
/// effects have been replicated.
pub(crate) async fn forward(
    state: &State,
    to: Forwarder<'_>,
    conn: &Connection,
    request: Request,
    recv: &mut RecvStream,
) -> Result<Reply, ErrorReply> {
    match request {
        Request::PutChunks { count } => {
            // The same limit the commit server applies, before anything is
            // read; the chunks are then passed on in bounded batches rather
            // than held all at once.
            crate::handler::check_batch(count as usize)?;
            let mut batch = Vec::new();
            let mut buffered = 0;
            for left in (0..count).rev() {
                let frame = expect_frame(recv).await.map_err(invalid)?;
                frame.expect(MsgType::Chunk).map_err(invalid)?;
                let (id, data) = split_chunk(&frame.payload).map_err(invalid)?;
                let id = ChunkId(kelp_core::Hash(id));
                if ChunkId::of(data) != id {
                    return Err(ErrorReply::new(
                        ErrorKind::Invalid,
                        format!("chunk {id} does not match its id"),
                    ));
                }
                buffered += data.len();
                batch.push((id, Bytes::copy_from_slice(data)));
                if buffered >= FORWARD_BYTES || left == 0 {
                    put_chunks(state, to, conn, std::mem::take(&mut batch)).await?;
                    buffered = 0;
                }
            }
            if !to.shares_storage() {
                state.blobs.sync().await.map_err(internal)?;
            }
            Ok(Reply::Ok)
        }
        Request::PutManifests { manifests } if !to.shares_storage() => {
            let ids: Vec<ContentId> = manifests.iter().map(|m| m.content).collect();
            let reply = conn
                .call(&Request::PutManifests { manifests })
                .await
                .map_err(upstream_error)?;
            // Keep what the commit server stored, which it verified, not
            // the client's claims, and never replace a manifest the edge
            // has.
            store_upstream_manifests(state, conn, &ids).await?;
            Ok(reply)
        }
        Request::HasChunks { .. } => conn.call(&request).await.map_err(upstream_error),
        request => {
            let reply = conn.call(&request).await.map_err(upstream_error)?;
            // The change is made; failing now would only make the client
            // repeat it.
            if let Err(e) = to.catch_up().await {
                tracing::warn!("a forwarded change is not visible here yet: {}", e.message);
            }
            Ok(reply)
        }
    }
}

/// Sends one batch of an upload on to where changes go, and keeps it here
/// too unless both share storage.
async fn put_chunks(
    state: &State,
    to: Forwarder<'_>,
    conn: &Connection,
    batch: Vec<(ChunkId, Bytes)>,
) -> Result<(), ErrorReply> {
    if batch.is_empty() {
        return Ok(());
    }
    conn.put_chunks(batch.clone())
        .await
        .map_err(upstream_error)?;
    if !to.shares_storage() {
        for (id, data) in batch {
            state.blobs.put(id, data).await.map_err(internal)?;
        }
    }
    Ok(())
}

/// Copies the commit server's manifests of `ids` that this server lacks,
/// asking over `conn`.
async fn store_upstream_manifests(
    state: &State,
    conn: &Connection,
    ids: &[ContentId],
) -> Result<(), ErrorReply> {
    let missing = {
        let reader = state.reader()?;
        let mut missing = Vec::new();
        for id in ids {
            if reader.manifest(id).map_err(internal)?.is_none() {
                missing.push(*id);
            }
        }
        missing
    };
    if missing.is_empty() {
        return Ok(());
    }
    let request = Request::GetManifests { ids: missing };
    let Reply::Manifests(found) = conn.call(&request).await.map_err(upstream_error)? else {
        return Err(unexpected());
    };
    let found: Vec<_> = found
        .into_iter()
        .flatten()
        .filter(|m| ids.contains(&m.content))
        .collect();
    state.db.put_manifests(&found).map_err(internal)
}

/// Makes sure the edge has the manifests of `ids` that the commit server has.
pub(crate) async fn fill_manifests(state: &State, ids: &[ContentId]) -> Result<(), ErrorReply> {
    let Some(edge) = state.edge() else {
        return Ok(());
    };
    let reader = state.reader()?;
    let mut missing = Vec::new();
    for id in ids {
        if reader.manifest(id).map_err(internal)?.is_none() {
            missing.push(*id);
        }
    }
    drop(reader);
    if missing.is_empty() {
        return Ok(());
    }
    let Reply::Manifests(found) = edge.call(&Request::GetManifests { ids: missing }).await? else {
        return Err(unexpected());
    };
    let found: Vec<_> = found.into_iter().flatten().collect();
    state.db.put_manifests(&found).map_err(internal)
}

/// Makes sure the edge has the chunks `ids` that the commit server has.
pub(crate) async fn fill_chunks(state: &State, ids: &[ChunkId]) -> Result<(), ErrorReply> {
    let Some(edge) = state.edge() else {
        return Ok(());
    };
    let have = state.blobs.has(ids).await.map_err(internal)?;
    let missing: Vec<ChunkId> = ids
        .iter()
        .zip(have)
        .filter(|(_, h)| !h)
        .map(|(id, _)| *id)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let conn = edge.connection().await?;
    for (id, data) in conn.get_chunks(&missing).await.map_err(upstream_error)? {
        state.blobs.put(id, data).await.map_err(internal)?;
    }
    state.blobs.sync().await.map_err(internal)
}

pub(crate) fn upstream_error(e: kelp_client::Error) -> ErrorReply {
    match e {
        kelp_client::Error::Remote(e) => e,
        e => ErrorReply::new(
            ErrorKind::Unavailable,
            format!("cannot reach the commit server: {e}"),
        ),
    }
}

fn unexpected() -> ErrorReply {
    ErrorReply::new(
        ErrorKind::Internal,
        "unexpected reply from the commit server",
    )
}

fn invalid(e: impl std::fmt::Display) -> ErrorReply {
    ErrorReply::new(ErrorKind::Invalid, e.to_string())
}

fn internal(e: impl std::fmt::Display) -> ErrorReply {
    ErrorReply::new(ErrorKind::Internal, e.to_string())
}
