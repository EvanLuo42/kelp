//! Exclusive file locks.

use serde::{Deserialize, Serialize};

/// The name of a family of streams that share locks.
///
/// A mainline and its development, virtual and task descendants form one
/// domain, so a binary asset cannot be edited concurrently on two branches
/// and diverge. Release streams start their own domain.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LockDomain(pub String);

/// The owner of a lock: a user working in a particular workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockHolder {
    /// The user name.
    pub user: String,
    /// The workspace name.
    pub workspace: String,
}

/// A held lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    /// The stream-relative path as spelled when the lock was taken.
    pub path: String,
    /// Who holds the lock.
    pub holder: LockHolder,
}
