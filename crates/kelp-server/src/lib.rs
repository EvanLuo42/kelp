//! The Kelp server.
//!
//! A [`Server`] owns a data directory holding the metadata database, the chunk
//! store and the TLS identity, and serves the `kelp/1` protocol over QUIC.
//! By default a server is all-in-one: one process is both the commit
//! authority and the storage node. With [`Config::upstream`] set it is an
//! edge instead, a replica of a commit server that serves reads locally and
//! forwards writes. With [`Config::cluster`] set it is a member of a commit
//! group, several servers that agree on the metadata log with Raft and
//! share one object store.
//!
//! # Data directory layout
//!
//! ```text
//! <data>/meta.redb      metadata database and command log
//! <data>/chunks/packs/  small chunks packed into append-only files, and their index
//! <data>/chunks/loose/  large chunks, one file each
//! <data>/tls/cert.der   self-signed certificate
//! <data>/tls/key.der    certificate private key (PKCS#8)
//! <data>/server.toml    optional settings; see [`StorageConfig`]
//! <data>/cluster.toml   commit group membership; see [`ClusterConfig`]
//! <data>/raft.redb      a commit group member's Raft log
//! <data>/raft-snapshot/ its latest snapshot
//! ```
//!
//! Chunks can live in an S3-compatible bucket instead, or in local storage
//! for current files with history in a bucket:
//!
//! ```toml
//! [storage]
//! kind = "tiered"
//!
//! [storage.s3]
//! bucket = "studio-kelp"
//! region = "eu-central-1"
//! ```

mod auth;
pub mod backup;
mod cluster;
mod edge;
pub mod fsck;
mod handler;
mod metrics;
mod oidc;
mod plugins;
mod role;
pub mod service;
mod storage;
mod tls;
mod webhook;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kelp_core::time::{Clock, SystemClock};
use kelp_meta::db::{Applied, Outcome, Proposal};
use kelp_meta::{Command, LogIndex, MetaDb};
use kelp_proto::{ErrorKind, ErrorReply};

pub use auth::totp_code;
pub use cluster::ClusterConfig;
pub use edge::Upstream;
pub use oidc::OidcConfig;
pub use storage::{Blobs, StorageConfig, StorageKind};
pub use tls::Identity;
pub use webhook::signature;

/// An error that prevents the server from starting or running.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("data directory {0} is not initialised; run `kelp-server init` first")]
    NotInitialised(PathBuf),
    #[error("data directory {0} already has users")]
    AlreadyInitialised(PathBuf),
    #[error("configuration: {0}")]
    Config(String),
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error(transparent)]
    Meta(#[from] kelp_meta::Error),
    #[error(transparent)]
    Cas(#[from] kelp_cas::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A specialized [`Result`](std::result::Result) type for server operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Settings for a [`Server`].
#[derive(Clone)]
pub struct Config {
    /// The data directory created by [`Server::init`].
    pub data_dir: PathBuf,
    /// The UDP address to listen on.
    pub listen: SocketAddr,
    /// How long issued access tokens remain valid.
    pub token_ttl: Duration,
    /// The source of timestamps for changelists and token expiry.
    pub clock: Arc<dyn Clock>,
    /// Where chunks are stored; [`Server::bind`] reads it from
    /// `<data>/server.toml` unless set.
    pub storage: Option<StorageConfig>,
    /// How long content a client has asked about or uploaded is protected
    /// from garbage collection, so that its submit can still use it.
    pub lease: Duration,
    /// Makes the server an edge of this commit server: it replicates the
    /// commit server's log, serves reads itself and forwards writes.
    pub upstream: Option<Upstream>,
    /// How often the server looks for webhook deliveries that are due.
    pub webhook_poll: Duration,
    /// How long the server waits before retrying a failed webhook delivery
    /// the first time; the wait doubles with each attempt.
    pub webhook_retry: Duration,
    /// Single sign-on settings; [`Server::bind`] reads them from
    /// `<data>/server.toml` unless set.
    pub oidc: Option<OidcConfig>,
    /// Stops each garbage collection between marking and deleting, for
    /// tests of what happens meanwhile.
    pub gc_pause: Option<GcPause>,
    /// Makes the server a member of a commit group; [`Server::bind`] reads
    /// it from `<data>/cluster.toml` unless set. Members must store chunks
    /// in one shared bucket.
    pub cluster: Option<ClusterConfig>,
    /// Where to serve `/healthz` and Prometheus `/metrics` over plain HTTP,
    /// if anywhere.
    pub metrics: Option<SocketAddr>,
}

/// A point where garbage collection waits; see [`Config::gc_pause`].
#[derive(Clone, Default)]
pub struct GcPause {
    /// Notified when a collection has marked what is live.
    pub marked: Arc<tokio::sync::Notify>,
    /// The collection continues once this is notified.
    pub resume: Arc<tokio::sync::Notify>,
}

impl Config {
    /// Creates a configuration with default settings for `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>, listen: SocketAddr) -> Self {
        Self {
            data_dir: data_dir.into(),
            listen,
            token_ttl: Duration::from_secs(30 * 24 * 3600),
            clock: Arc::new(SystemClock),
            lease: Duration::from_secs(3600),
            storage: None,
            upstream: None,
            webhook_poll: Duration::from_secs(1),
            webhook_retry: Duration::from_secs(10),
            oidc: None,
            gc_pause: None,
            cluster: None,
            metrics: None,
        }
    }
}

pub(crate) struct State {
    pub db: Arc<MetaDb>,
    pub blobs: Blobs,
    pub clock: Arc<dyn Clock>,
    pub token_ttl: Duration,
    pub leases: handler::Leases,
    pub started_at: u64,
    pub gc_lock: tokio::sync::Mutex<()>,
    /// Held for reading while content is checked or uploaded, and for
    /// writing while garbage collection deletes content.
    pub sweep: tokio::sync::RwLock<()>,
    pub gc_pause: Option<GcPause>,
    pub role: role::Role,
    /// Requests handled, and those that failed, since the server started.
    pub requests: std::sync::atomic::AtomicU64,
    pub failures: std::sync::atomic::AtomicU64,
    pub plugins: plugins::Plugins,
    pub data_dir: PathBuf,
    pub oidc: Option<oidc::Oidc>,
    /// The last TOTP time step each user logged in with, against replays.
    pub totp_steps: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl State {
    /// Returns a consistent read snapshot of the metadata.
    pub(crate) fn reader(&self) -> Result<kelp_meta::db::Reader, ErrorReply> {
        self.db
            .read()
            .map_err(|e| ErrorReply::new(ErrorKind::Internal, e.to_string()))
    }

    /// Applies `proposal` and returns what it produced.
    pub(crate) async fn propose(&self, proposal: Proposal) -> Result<Outcome, ErrorReply> {
        if let Some(cluster) = self.cluster() {
            return cluster.propose(proposal).await;
        }
        if self.edge().is_some() {
            // Its database is a copy of the commit server's log; a write of
            // its own would end replication for good.
            return Err(ErrorReply::new(
                ErrorKind::Internal,
                "an edge cannot change metadata itself",
            ));
        }
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.propose(&proposal))
            .await
            .map_err(|e| ErrorReply::new(ErrorKind::Internal, e.to_string()))?
            .map_err(handler::meta_error)
    }

    /// Applies `commands` on behalf of `actor` in one transaction.
    pub(crate) async fn commit_all(
        &self,
        actor: &str,
        commands: Vec<Command>,
    ) -> Result<(), ErrorReply> {
        let batch = commands
            .into_iter()
            .map(|c| (actor.to_owned(), c))
            .collect();
        self.propose(Proposal::Batch(batch)).await.map(|_| ())
    }

    /// Applies `cmd` on behalf of `actor`.
    pub(crate) async fn commit(
        &self,
        actor: &str,
        cmd: impl Into<Command>,
    ) -> Result<(LogIndex, Applied), ErrorReply> {
        let proposal = Proposal::Command {
            actor: actor.to_owned(),
            command: cmd.into(),
        };
        Ok(self.propose(proposal).await?.logged())
    }
}

/// A running Kelp server.
pub struct Server {
    endpoint: quinn::Endpoint,
    identity: Identity,
    metrics: Option<std::net::TcpListener>,
    cluster: Option<ClusterConfig>,
    state: Arc<State>,
    webhook_poll: Duration,
    webhook_retry: Duration,
}

impl Server {
    /// Initialises `data_dir` with a metadata database, a TLS identity and a
    /// first administrator account.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AlreadyInitialised`] if the directory already has users,
    /// or an error if the administrator account is invalid or the directory
    /// cannot be written.
    pub fn init(
        data_dir: &Path,
        admin: &str,
        password: &str,
        clock: &dyn Clock,
    ) -> Result<Identity> {
        std::fs::create_dir_all(data_dir)?;
        let db = MetaDb::open(&data_dir.join("meta.redb"))?;
        if db.read()?.has_users()? {
            return Err(Error::AlreadyInitialised(data_dir.to_owned()));
        }
        let identity = Identity::load_or_create(&data_dir.join("tls"))?;
        db.commit(Command::CreateUser {
            name: admin.to_owned(),
            password_hash: Some(auth::hash_password(password)),
            admin: true,
            replica: false,
            at: clock.now(),
        })?;
        Ok(identity)
    }

    /// Opens the data directory and starts listening.
    ///
    /// The returned server does not accept connections until [`Server::run`]
    /// is called.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotInitialised`] if [`Server::init`] has not been run on
    /// the data directory, or an error if the socket cannot be bound.
    pub fn bind(config: Config) -> Result<Self> {
        let meta_path = config.data_dir.join("meta.redb");
        let cluster = match config.cluster.clone() {
            Some(c) => Some(c),
            None => ClusterConfig::load(&config.data_dir)?,
        };
        if !meta_path.exists() && config.upstream.is_none() && cluster.is_none() {
            return Err(Error::NotInitialised(config.data_dir));
        }
        std::fs::create_dir_all(&config.data_dir)?;
        let storage = match &config.storage {
            Some(s) => s.clone(),
            None => StorageConfig::load(&config.data_dir)?,
        };
        if cluster.is_some() && storage.kind != StorageKind::S3 {
            return Err(Error::Config(
                "commit group members share chunks through object storage; set [storage] kind = \"s3\"".into(),
            ));
        }
        let blobs = Blobs::open(&storage, &config.data_dir.join("chunks"))?;
        let oidc = match config.oidc.clone() {
            Some(o) => Some(o),
            None => OidcConfig::load(&config.data_dir)?,
        };
        let identity = Identity::load_or_create(&config.data_dir.join("tls"))?;
        let endpoint = quinn::Endpoint::server(identity.server_config()?, config.listen)?;
        let db = Arc::new(MetaDb::open(&meta_path)?);
        if cluster.is_some() {
            cluster::upgrade_log(&config.data_dir, &db)?;
        }
        let applied = db.read()?.last_index()?.0;
        let role = match (&cluster, config.upstream) {
            (Some(_), _) => role::Role::Member(role::Membership::new(identity.fingerprint())),
            (None, Some(u)) => role::Role::Edge(Box::new(edge::Edge::new(u, applied))),
            (None, None) => role::Role::AllInOne,
        };
        let metrics = match config.metrics {
            Some(addr) => {
                let listener = std::net::TcpListener::bind(addr)?;
                listener.set_nonblocking(true)?;
                Some(listener)
            }
            None => None,
        };
        let state = State {
            role,
            requests: Default::default(),
            failures: Default::default(),
            plugins: plugins::Plugins::new(),
            data_dir: config.data_dir.clone(),
            oidc: oidc.map(oidc::Oidc::new),
            totp_steps: Default::default(),
            db,
            blobs,
            leases: handler::Leases::new(config.lease),
            started_at: config.clock.now(),
            gc_lock: tokio::sync::Mutex::new(()),
            sweep: tokio::sync::RwLock::new(()),
            gc_pause: config.gc_pause.clone(),
            clock: config.clock,
            token_ttl: config.token_ttl,
        };
        Ok(Self {
            endpoint,
            identity,
            metrics,
            cluster,
            state: Arc::new(state),
            webhook_poll: config.webhook_poll,
            webhook_retry: config.webhook_retry,
        })
    }

    /// Returns the address the server is listening on.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket address cannot be queried.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Returns the address of the monitoring endpoint, if it has one.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics.as_ref().and_then(|l| l.local_addr().ok())
    }

    /// Returns the server's data directory.
    pub fn data_dir(&self) -> &Path {
        &self.state.data_dir
    }

    /// Returns the server's TLS identity, whose fingerprint clients pin.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Returns a handle that stops the server when [`ShutdownHandle::shutdown`] is called.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            endpoint: self.endpoint.clone(),
        }
    }

    /// Accepts and serves connections until the server is shut down.
    pub async fn run(self) {
        if let Some(config) = self.cluster.clone() {
            let started = cluster::Cluster::start(
                config,
                &self.state.data_dir,
                self.state.db.clone(),
                self.identity.fingerprint(),
                self.state.clock.clone(),
            )
            .await;
            match (started, &self.state.role) {
                (Ok(c), role::Role::Member(membership)) => membership.set(Arc::new(c)),
                (Ok(_), _) => unreachable!("only members start a commit group"),
                (Err(e), _) => {
                    tracing::error!("cannot join the commit group: {e}");
                    return;
                }
            }
        }
        let monitoring = match self.metrics {
            Some(listener) => match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => Some(tokio::spawn(metrics::serve(self.state.clone(), listener))),
                Err(e) => {
                    tracing::error!("cannot serve metrics: {e}");
                    None
                }
            },
            None => None,
        };
        let background = match self.state.edge() {
            Some(_) => tokio::spawn(edge::replicate(self.state.clone())),
            _ => tokio::spawn(webhook::deliver(
                self.state.clone(),
                self.webhook_poll,
                self.webhook_retry,
            )),
        };
        while let Some(incoming) = self.endpoint.accept().await {
            let state = self.state.clone();
            let peer = incoming.remote_address();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => handler::serve_connection(state, conn).await,
                    Err(e) => tracing::info!(%peer, "connection failed: {e}"),
                }
            });
        }
        background.abort();
        if let Some(monitoring) = monitoring {
            monitoring.abort();
        }
        if let Some(cluster) = self.state.cluster() {
            cluster.shutdown().await;
        }
    }
}

/// Stops a running [`Server`].
#[derive(Clone)]
pub struct ShutdownHandle {
    endpoint: quinn::Endpoint,
}

impl ShutdownHandle {
    /// Closes all connections and makes [`Server::run`] return.
    pub fn shutdown(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}
