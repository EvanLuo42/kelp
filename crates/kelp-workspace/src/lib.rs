//! The client-side workspace model.
//!
//! This crate holds the data a workspace keeps between commands: which files
//! the workspace has ([`State::have`]), which files are opened in which pending
//! changelist ([`PendingChanges`]), how depot paths map onto the disk
//! ([`View`]) and which local files to ignore ([`Ignore`]). It performs no
//! network access; synchronising with a server is the job of `kelp-client`.

pub mod changelist;
pub mod hunk;
pub mod ignore;
pub mod state;
pub mod view;

pub use changelist::{OpenAction, PendingChangelist, PendingChanges, Selection};
pub use hunk::{Hunk, HunkAnchor};
pub use ignore::Ignore;
pub use state::{
    ArtifactHave, ArtifactMount, Config, FileVersion, HaveEntry, META_DIR, Resolve, ResolveKind,
    STATE_VERSION, Snapshot, SnapshotFile, State,
};
pub use view::View;

/// An error from the workspace model.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no pending changelist {0}")]
    NoSuchChangelist(kelp_core::LocalClId),
    #[error("{0} is not opened")]
    NotOpened(kelp_core::DepotPath),
    #[error("{0} cannot be split by hunk (binary or exclusively locked)")]
    NotSplittable(kelp_core::DepotPath),
    #[error("invalid view line {0:?}: {1}")]
    InvalidView(String, &'static str),
    #[error("invalid artifact directory {0:?}: it must be relative and inside the workspace")]
    InvalidArtifactMount(String),
    #[error("invalid ignore pattern {0:?}: {1}")]
    InvalidIgnore(String, String),
    #[error("corrupt workspace file {0}: {1}")]
    Corrupt(std::path::PathBuf, String),
    #[error(transparent)]
    Core(#[from] kelp_core::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A specialized [`Result`](std::result::Result) type for workspace operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;
