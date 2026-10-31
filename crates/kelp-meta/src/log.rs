//! Positions in the replicated command log.

use serde::{Deserialize, Serialize};

/// A position in the replicated command log.
///
/// Clients carry the highest index they have observed so that edges can
/// provide read-your-writes consistency.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct LogIndex(pub u64);

/// One command of the log, as the commit server sends it to edge servers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub index: u64,
    /// The user on whose behalf the command was applied.
    pub actor: String,
    /// The encoded [`Command`](crate::Command).
    pub command: Vec<u8>,
    /// The audit hash after this entry; see
    /// [`AuditEntry`](crate::record::AuditEntry).
    pub hash: kelp_core::Hash,
}
