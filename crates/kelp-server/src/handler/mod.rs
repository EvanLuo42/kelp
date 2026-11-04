mod account;
mod admin;
mod artifact;
mod change;
mod content;
mod dispatch;
mod issue;
mod label;
mod plugin;
mod review;
mod stream;
mod webhook;

use std::sync::{Arc, Mutex};

use kelp_client::Connection;
use kelp_core::{ContentId, DepotPath};
use kelp_meta::acl::{Level, Protections, Role, Subject};
use kelp_meta::db::Reader;
use kelp_meta::record::User;
use kelp_proto::io::{expect_frame_within, write_message};
use kelp_proto::{
    Auth, Capabilities, ErrorKind, ErrorReply, Hello, MsgType, PROTOCOL_VERSION, Reply, Request,
    RequestClass, Welcome,
};
use quinn::{RecvStream, SendStream};
use tracing::Instrument;

use crate::edge;
use crate::role::Forwarder;
use crate::{State, auth};

const MAX_BATCH: usize = 4096;

type Response = Result<Reply, ErrorReply>;

pub use admin::Leases;

/// Per-connection state.
#[derive(Default)]
pub(crate) struct Session {
    user: Mutex<Option<User>>,
    /// The token the client authenticated with, which an edge presents to the
    /// commit server on the client's behalf.
    token: Mutex<Option<String>>,
    /// An edge's connection to the commit server as this client's user.
    upstream: tokio::sync::Mutex<Option<Connection>>,
    /// Whether the peer is a member of this server's commit group, shown by
    /// a Raft message with the group's secret.
    member: std::sync::atomic::AtomicBool,
}

impl Session {
    /// Returns `true` if the peer may send messages larger than a frame:
    /// it has logged in or shown that it is a member of the commit group.
    fn trusted(&self) -> bool {
        self.member.load(std::sync::atomic::Ordering::Relaxed)
            || self.user.lock().expect("session lock").is_some()
    }

    fn user(&self) -> Result<User, ErrorReply> {
        self.user
            .lock()
            .expect("session lock")
            .clone()
            .ok_or_else(|| ErrorReply::new(ErrorKind::Unauthenticated, "log in first"))
    }

    fn set_user(&self, user: Option<User>) {
        *self.user.lock().expect("session lock") = user;
    }

    fn set_token(&self, token: Option<String>) {
        *self.token.lock().expect("session lock") = token;
    }

    /// Returns the connection to where changes go, as this session's user.
    async fn upstream(&self, to: Forwarder<'_>) -> Result<Connection, ErrorReply> {
        let mut upstream = self.upstream.lock().await;
        if let Some(c) = upstream.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let token = self.token.lock().expect("session lock").clone();
        let token = token.ok_or_else(|| {
            ErrorReply::new(ErrorKind::Unauthenticated, "log in to the edge first")
        })?;
        let conn = to.connect(Auth::Bearer(token)).await?;
        *upstream = Some(conn.clone());
        Ok(conn)
    }
}

/// What the current user may do, evaluated against one read snapshot.
pub(crate) struct Access {
    pub(super) user: User,
    groups: Vec<String>,
    table: Protections,
}

impl Access {
    fn load(reader: &Reader, user: User) -> Result<Self, ErrorReply> {
        if reader.is_disabled(&user.name).map_err(meta_error)? {
            return Err(disabled(&user.name));
        }
        Ok(Self {
            groups: reader.groups_of(&user.name).map_err(meta_error)?,
            table: reader.protections().map_err(meta_error)?,
            user,
        })
    }

    fn name(&self) -> &str {
        &self.user.name
    }

    fn is_admin(&self) -> bool {
        self.user.admin
    }

    fn is_replica(&self) -> bool {
        self.user.replica
    }

    fn subject(&self) -> Subject<'_> {
        Subject {
            user: &self.user.name,
            groups: &self.groups,
        }
    }

    fn can(&self, level: Level, path: &DepotPath) -> bool {
        self.user.admin || self.table.level(self.subject(), path) >= level
    }

    fn require(&self, level: Level, path: &DepotPath) -> Result<(), ErrorReply> {
        if self.can(level, path) {
            return Ok(());
        }
        let verb = if level == Level::Write {
            "write"
        } else {
            "read"
        };
        Err(ErrorReply::new(
            ErrorKind::Forbidden,
            format!("no permission to {verb} {path}"),
        ))
    }

    fn sees(&self, stream: &DepotPath) -> bool {
        self.user.admin || self.table.may_read_below(self.subject(), stream)
    }

    fn require_stream(&self, stream: &DepotPath) -> Result<(), ErrorReply> {
        match self.sees(stream) {
            true => Ok(()),
            false => Err(ErrorReply::new(
                ErrorKind::NotFound,
                format!("stream {stream} not found"),
            )),
        }
    }

    fn require_admin(&self) -> Result<(), ErrorReply> {
        match self.user.admin {
            true => Ok(()),
            false => Err(ErrorReply::new(
                ErrorKind::Forbidden,
                "administrator required",
            )),
        }
    }

    /// Returns `true` if the user is a server administrator or a `role` line
    /// grants them `role`.
    fn has_role(&self, role: Role) -> bool {
        self.user.admin || self.table.has_role(self.subject(), role)
    }

    fn require_role(&self, role: Role) -> Result<(), ErrorReply> {
        match self.has_role(role) {
            true => Ok(()),
            false => Err(ErrorReply::new(
                ErrorKind::Forbidden,
                format!("administrator or the {role} role required"),
            )),
        }
    }

    /// Requires the accounts role for managing `target`. Server
    /// administrators and replicas are managed only by server
    /// administrators, so the role cannot be used to gain their powers.
    fn require_accounts_over(&self, reader: &Reader, target: &str) -> Result<(), ErrorReply> {
        if self.user.admin {
            return Ok(());
        }
        self.require_role(Role::Accounts)?;
        let current = reader
            .alias(target)
            .map_err(meta_error)?
            .unwrap_or_else(|| target.to_owned());
        match reader.user(&current).map_err(meta_error)? {
            Some(u) if u.admin || u.replica => Err(ErrorReply::new(
                ErrorKind::Forbidden,
                format!("{target} is a server administrator or replica; ask an administrator"),
            )),
            _ => Ok(()),
        }
    }

    /// Returns `true` if the user administers the directory `dir`, such as a
    /// depot or a stream: as a server administrator, or through an `admin`
    /// line that covers all of it.
    fn administers(&self, dir: &DepotPath) -> bool {
        self.user.admin || self.table.dir_level(self.subject(), dir) == Level::Admin
    }

    fn require_administers(&self, dir: &DepotPath) -> Result<(), ErrorReply> {
        match self.administers(dir) {
            true => Ok(()),
            false => Err(ErrorReply::new(
                ErrorKind::Forbidden,
                format!("administrator of {dir} required"),
            )),
        }
    }

    /// Returns `true` if the user may read every path, so reading content
    /// by its hash needs no check of where it appears.
    fn reads_everything(&self) -> bool {
        self.user.admin || self.user.replica || self.table.reads_everything(self.subject())
    }

    /// Returns `true` if the user may read `content`: it appears at a path
    /// they can read, in an artifact of a stream they see, or in a plugin.
    fn reads_content(&self, reader: &Reader, content: &ContentId) -> Result<bool, ErrorReply> {
        if self.reads_everything() {
            return Ok(true);
        }
        reader
            .content_visible(content, |p| self.can(Level::Read, p), |s| self.sees(s))
            .map_err(meta_error)
    }
}

pub async fn serve_connection(state: Arc<State>, conn: quinn::Connection) {
    let span = tracing::info_span!("conn", id = conn.stable_id(), peer = %conn.remote_address());
    tracing::debug!(parent: &span, "connected");
    let session = Arc::new(Session::default());
    let closed = loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let (state, session) = (state.clone(), session.clone());
                tokio::spawn(
                    async move {
                        if let Err(e) = serve_stream(&state, &session, send, recv).await {
                            tracing::debug!("stream failed: {e}");
                        }
                    }
                    .instrument(span.clone()),
                );
            }
            Err(e) => break e,
        }
    };
    tracing::debug!(parent: &span, "disconnected: {closed}");
}

async fn serve_stream(
    state: &Arc<State>,
    session: &Session,
    mut send: SendStream,
    mut recv: RecvStream,
) -> kelp_proto::Result<()> {
    // Only a known peer may make the server hold a message larger than a
    // frame.
    let limit = match session.trusted() {
        true => kelp_proto::MAX_MESSAGE,
        false => kelp_proto::MAX_PAYLOAD,
    };
    let frame = expect_frame_within(&mut recv, limit).await?;
    let response = match frame.header.msg_type {
        MsgType::Hello => {
            let hello: Hello = frame.decode()?;
            match hello_reply(state, session, &hello).await {
                Ok(welcome) => {
                    tracing::debug!(user = welcome.user.as_deref(), "hello");
                    write_message(&mut send, MsgType::Welcome, &welcome).await?
                }
                Err(e) => {
                    tracing::warn!(kind = ?e.kind, "hello refused: {}", e.message);
                    write_message(&mut send, MsgType::Error, &e).await?
                }
            }
            None
        }
        MsgType::Request => {
            let request: Request = frame.decode()?;
            let (op, class) = (request.name(), request.class());
            let attempt = match &request {
                Request::Login { user, .. } => Some(user.clone()),
                _ => None,
            };
            let started = std::time::Instant::now();
            let response = handle(state, session, request, &mut send, &mut recv).await;
            log_request(session, attempt, op, class, started.elapsed(), &response);
            state
                .requests
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if response.is_err() {
                state
                    .failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Some(response)
        }
        other => Some(Err(ErrorReply::new(
            ErrorKind::Invalid,
            format!("unexpected {other:?} frame"),
        ))),
    };
    match response {
        Some(Ok(reply)) => write_message(&mut send, MsgType::Reply, &reply).await?,
        Some(Err(e)) => write_message(&mut send, MsgType::Error, &e).await?,
        None => {}
    }
    let _ = send.finish();
    let _ = send.stopped().await;
    Ok(())
}

/// Logs a handled request: logins and writes at `info`, everything else at
/// `debug`, and failures by how much an operator should care. `attempt` is
/// the user a login names, which the session has not taken on if it fails.
fn log_request(
    session: &Session,
    attempt: Option<String>,
    op: &str,
    class: RequestClass,
    elapsed: std::time::Duration,
    response: &Response,
) {
    let user = session.user.lock().expect("session lock");
    let user = user
        .as_ref()
        .map(|u| u.name.as_str())
        .or(attempt.as_deref());
    let ms = elapsed.as_millis() as u64;
    match response {
        // Single sign-on clients poll, so only password logins are worth `info`.
        Ok(_) if class == RequestClass::Write || op == "Login" => {
            tracing::info!(op, user, ms, "request done")
        }
        Ok(_) => tracing::debug!(op, user, ms, "request done"),
        Err(e) => match e.kind {
            ErrorKind::Internal => {
                tracing::error!(op, user, ms, kind = ?e.kind, "request failed: {}", e.message)
            }
            ErrorKind::Unauthenticated | ErrorKind::Forbidden | ErrorKind::Unavailable => {
                tracing::warn!(op, user, ms, kind = ?e.kind, "request failed: {}", e.message)
            }
            _ => tracing::info!(op, user, ms, kind = ?e.kind, "request failed: {}", e.message),
        },
    }
}

async fn hello_reply(
    state: &State,
    session: &Session,
    hello: &Hello,
) -> Result<Welcome, ErrorReply> {
    let version = hello.negotiate(1, PROTOCOL_VERSION).ok_or_else(|| {
        ErrorReply::new(
            ErrorKind::Invalid,
            format!("server speaks protocol version {PROTOCOL_VERSION}"),
        )
    })?;
    let user = match &hello.auth {
        Auth::None => None,
        Auth::Bearer(token) => {
            let hash = auth::token_hash(token);
            let mut record = state.reader()?.token(&hash).map_err(internal)?;
            if record.is_none() && state.forwarder().is_some() {
                // The token may have been issued moments ago.
                state.catch_up().await?;
                record = state.reader()?.token(&hash).map_err(internal)?;
            }
            let record = record
                .filter(|t| t.expires_at > state.clock.now())
                .ok_or_else(|| {
                    ErrorReply::new(ErrorKind::Unauthenticated, "token is invalid or expired")
                })?;
            let reader = state.reader()?;
            let user = reader.user(&record.user).map_err(internal)?;
            let user = user.ok_or_else(|| {
                ErrorReply::new(ErrorKind::Unauthenticated, "user no longer exists")
            })?;
            if reader.is_disabled(&user.name).map_err(meta_error)? {
                return Err(disabled(&user.name));
            }
            Some(user)
        }
    };
    let name = user.as_ref().map(|u| u.name.clone());
    session.set_user(user);
    session.set_token(match &hello.auth {
        Auth::Bearer(token) => Some(token.clone()),
        Auth::None => None,
    });
    let reader = state.reader()?;
    Ok(Welcome {
        version,
        role: state.server_role(),
        log_index: reader.last_index().map_err(internal)?.0,
        capabilities: Capabilities::CONTENT_ACCESS,
        user: name,
    })
}

/// Handles `request`, sending it on to the new leader if this member lost
/// the lead of its commit group before it could log the change.
async fn handle(
    state: &Arc<State>,
    session: &Session,
    request: Request,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Response {
    // Content streamed with the request cannot be read twice, and a device
    // login's code may already be redeemed.
    let retryable = match request {
        Request::PutChunks { .. } => false,
        Request::Login { .. } => true,
        _ => matches!(request.class(), RequestClass::Write | RequestClass::Upload),
    };
    let again = match state.cluster() {
        Some(_) if retryable => Some(request.clone()),
        _ => None,
    };
    match handle_once(state, session, request, send, recv).await {
        Err(e) if e.kind == ErrorKind::NotLeader && state.forwarder().is_some() => match again {
            Some(request) => handle_once(state, session, request, send, recv).await,
            None => Err(e),
        },
        result => result,
    }
}

async fn handle_once(
    state: &Arc<State>,
    session: &Session,
    request: Request,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Response {
    if let Request::Raft {
        secret,
        format,
        rpc,
        payload,
    } = &request
    {
        let cluster = state.cluster().ok_or_else(|| {
            ErrorReply::new(ErrorKind::Forbidden, "this server is not in a commit group")
        })?;
        let reply = cluster.serve(secret, *format, *rpc, payload).await?;
        session
            .member
            .store(true, std::sync::atomic::Ordering::Relaxed);
        return Ok(Reply::Raft(reply));
    }
    if let Some(edge) = state.edge() {
        edge.check()?;
    }
    if matches!(
        request,
        Request::Login { .. } | Request::OidcStart | Request::OidcPoll { .. }
    ) {
        if let Some(to) = state.forwarder() {
            return account::login_through(state, to, session, request).await;
        }
        return match request {
            Request::Login {
                user,
                password,
                code,
            } => account::login(state, session, user, password, code).await,
            Request::OidcStart => account::oidc_start(state).await,
            Request::OidcPoll { device_code } => {
                account::oidc_poll(state, session, device_code).await
            }
            _ => unreachable!("matched above"),
        };
    }
    let user = session.user()?;
    let forwarded = matches!(
        request.class(),
        RequestClass::Write | RequestClass::Upload | RequestClass::Authoritative
    );
    if let Some(to) = state.forwarder()
        && forwarded
    {
        let conn = session.upstream(to).await?;
        return edge::forward(state, to, &conn, request, recv).await;
    }
    let access = Access::load(&state.reader()?, user)?;
    dispatch::dispatch(state, session, access, request, send, recv).await
}

fn parse_path(s: &str) -> Result<DepotPath, ErrorReply> {
    DepotPath::parse(s).map_err(invalid)
}

fn check_batch(len: usize) -> Result<(), ErrorReply> {
    match len {
        0..=MAX_BATCH => Ok(()),
        _ => Err(ErrorReply::new(
            ErrorKind::Invalid,
            format!("batch of {len} exceeds {MAX_BATCH}"),
        )),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ErrorReply> + Send + 'static,
) -> Result<T, ErrorReply> {
    tokio::task::spawn_blocking(f).await.map_err(internal)?
}

/// The error for a missing object, described as `what`.
pub(super) fn not_found(what: String) -> ErrorReply {
    ErrorReply::new(ErrorKind::NotFound, format!("{what} not found"))
}

/// The error for a disabled user's attempt to log in or make a request.
pub(super) fn disabled(user: &str) -> ErrorReply {
    ErrorReply::new(
        ErrorKind::Unauthenticated,
        format!("{user} is disabled; ask an administrator"),
    )
}

pub(crate) fn meta_error(e: kelp_meta::Error) -> ErrorReply {
    use kelp_meta::Error as E;
    let kind = match &e {
        E::NotFound(_) => ErrorKind::NotFound,
        E::AlreadyExists(_) => ErrorKind::AlreadyExists,
        E::Conflict(_) | E::PathConflict(_) | E::DuplicateEntry(_) | E::Locked(_) => {
            ErrorKind::Conflict
        }
        E::Invalid(_) | E::MissingContent(_) | E::Core(_) => ErrorKind::Invalid,
        _ => ErrorKind::Internal,
    };
    ErrorReply::new(kind, e.to_string())
}

fn invalid(e: impl std::fmt::Display) -> ErrorReply {
    ErrorReply::new(ErrorKind::Invalid, e.to_string())
}

fn internal(e: impl std::fmt::Display) -> ErrorReply {
    ErrorReply::new(ErrorKind::Internal, e.to_string())
}
