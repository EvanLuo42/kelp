//! The Kelp client library.
//!
//! [`Connection`] speaks the `kelp/1` protocol to a server, [`ContentCache`]
//! holds file content on the local machine, [`Home`] stores per-user settings
//! such as credentials, and [`Workspace`] implements the workspace commands
//! (sync, status, open, submit and so on) on top of them. [`artifact`]
//! publishes and fetches build artifacts.

pub mod artifact;
mod cache;
mod fsutil;
mod home;
mod index;
pub mod ipc;
mod net;
pub mod ops;
pub mod plugins;
pub mod progress;
mod workspace;

pub use cache::ContentCache;
pub use fsutil::is_read_only;
pub use home::{Credentials, Home, ServerEntry};
pub use index::{DiskEntry, DiskIndex};
pub use net::{Connection, Trust};
pub use workspace::{
    AnnotatedLine, ArtifactUpdate, CleanReport, Described, FileDiff, FileReport, MergeReport,
    ProjectedFile, Projector, ResolveMode, ResolveOutcome, SkipReason, SnapshotSide, Status,
    SyncReport, Workspace, annotate, read_file, render_diff,
};

use kelp_core::DepotPath;

/// An error from a client operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server rejected the request.
    #[error("server: {}", .0.message)]
    Remote(kelp_proto::ErrorReply),
    /// The server presented a different certificate than the pinned one.
    #[error(
        "server fingerprint changed from {expected} to {actual}; if this is expected, remove the server from \
         your credentials and log in again"
    )]
    UntrustedServer { expected: String, actual: String },
    #[error("cannot connect to {0}: {1}")]
    Connect(String, String),
    #[error("not logged in to {0}; run `kelp login {0}`")]
    NotLoggedIn(String),
    #[error("{0}")]
    Daemon(String),
    #[error("kelpd runs version {0}")]
    DaemonVersion(String),
    #[error("another Kelp process is using this workspace")]
    Busy,
    #[error("not inside a Kelp workspace")]
    NoWorkspace,
    #[error("{0} already contains a workspace")]
    WorkspaceExists(std::path::PathBuf),
    #[error("{0} is outside the workspace view")]
    OutsideView(String),
    #[error("{0} is not in the workspace")]
    NotTracked(DepotPath),
    #[error("{0} is already in the workspace")]
    AlreadyTracked(DepotPath),
    #[error("{0} is not opened")]
    NotOpened(DepotPath),
    /// A file has changes that the operation would destroy.
    #[error("{0} has changes that are not opened; open them, or move the file out of the way")]
    WouldOverwrite(DepotPath),
    #[error("content {0} is not in the local cache")]
    NotCached(kelp_core::ContentId),
    #[error("{0}")]
    Invalid(String),
    #[error("unexpected reply from server")]
    UnexpectedReply,
    #[error(transparent)]
    Protocol(#[from] kelp_proto::Error),
    #[error(transparent)]
    Workspace(#[from] kelp_workspace::Error),
    #[error(transparent)]
    Cas(#[from] kelp_cas::Error),
    #[error(transparent)]
    Plugin(#[from] kelp_plugin::Error),
    #[error(transparent)]
    Core(#[from] kelp_core::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A specialized [`Result`](std::result::Result) type for client operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;
