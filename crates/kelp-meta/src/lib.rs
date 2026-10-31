//! The metadata layer: users, depots, streams, changelists and file trees.
//!
//! Every write is a [`Command`] appended to a log and applied by a
//! deterministic state machine, [`MetaDb`]. The same log format backs an
//! all-in-one server, a commit node with edge replicas, and a Raft commit
//! group, so a deployment can grow without migrating data.

pub mod acl;
pub mod command;
pub mod db;
pub mod event;
pub mod governance;
pub mod lock;
pub mod log;
pub mod record;
pub mod review;
pub mod tree;

pub use command::{
    ArtifactContents, Command, FileAction, FileChange, FileData, Integration, ShelfContents,
    StreamKind, Submission, Timestamp,
};
pub use db::MetaDb;
pub use governance::Governance;
pub use lock::{Lock, LockDomain, LockHolder};
pub use log::{LogEntry, LogIndex};
pub use tree::{Dirs, EntryKind, PathDiff, TreeEntry};

/// The error type for metadata operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Some of the requested paths are locked by another holder.
    #[error("locked by others: {}", .0.join(", "))]
    Locked(Vec<String>),
    /// Two entries of one directory have names that are equal after case folding.
    #[error("duplicate entry {0:?} in directory (names differ only in case?)")]
    DuplicateEntry(String),
    /// A path would be both a file and a directory.
    #[error("{0:?} is both a file and a directory")]
    PathConflict(String),
    /// The object to be created already exists.
    #[error("{0} already exists")]
    AlreadyExists(String),
    /// The referenced object does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// A submit conflicts with the current state; one message per problem.
    #[error("submit conflicts: {}", .0.join("; "))]
    Conflict(Vec<String>),
    /// A submit references content whose manifest has not been uploaded.
    #[error("content not uploaded for: {}", .0.join(", "))]
    MissingContent(Vec<String>),
    /// The audit chain does not match the command log at this index.
    #[error("audit log does not match the command log at entry {0}")]
    AuditMismatch(u64),
    /// A command or argument is malformed.
    #[error("{0}")]
    Invalid(String),
    /// A directory entry could not be decoded.
    #[error("corrupt tree entry")]
    CorruptEntry,
    /// A record in the named table could not be decoded.
    #[error("corrupt record in table {0}")]
    CorruptRecord(&'static str),
    /// The database has a format this build cannot use.
    #[error("{0}")]
    Format(String),
    /// The underlying database failed.
    #[error("database error: {0}")]
    Db(String),
    /// A prolly tree operation failed.
    #[error(transparent)]
    Prolly(#[from] kelp_prolly::Error),
    /// Core parsing or validation failed.
    #[error(transparent)]
    Core(#[from] kelp_core::Error),
}

macro_rules! db_errors {
    ($($t:ty),*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Self::Db(e.to_string())
            }
        }
    )*};
}
db_errors!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);

/// A specialized [`Result`](std::result::Result) type whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
