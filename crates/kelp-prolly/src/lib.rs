//! Prolly trees: content-defined B-trees over sorted byte keys.
//!
//! Node boundaries depend only on the entries themselves, so a given set of
//! entries always produces the same tree and root hash regardless of the order
//! of edits that built it. This property is called history independence.
//! Nodes have bounded size, edits rewrite O(log n) nodes, and diffs skip
//! identical subtrees.
//!
//! Prolly trees back the directory listings of the Merkle file tree and the
//! metadata indexes that clients and edges sync incrementally.
//!
//! # Examples
//!
//! ```
//! use kelp_prolly::{ChunkConfig, Diff, MemStore, Prolly};
//!
//! let store = MemStore::default();
//! let tree = Prolly::new(&store, ChunkConfig::DEFAULT);
//!
//! let a = tree.build([(b"a".to_vec(), b"1".to_vec())]).unwrap();
//! let b = tree.apply(a, [(b"b".to_vec(), Some(b"2".to_vec()))]).unwrap();
//!
//! assert_eq!(tree.get(b, b"b").unwrap(), Some(b"2".to_vec()));
//! assert_eq!(
//!     tree.diff(a, b).unwrap(),
//!     vec![Diff::Added(b"b".to_vec(), b"2".to_vec())]
//! );
//! ```

mod chunker;
mod cursor;
mod diff;
mod node;
mod store;
mod tree;

pub use chunker::ChunkConfig;
pub use diff::Diff;
pub use node::{Node, NodeId};
pub use store::{MemStore, NodeStore};
pub use tree::Prolly;

/// A tree key. Entries are ordered by comparing keys bytewise.
pub type Key = Vec<u8>;

/// A tree value.
pub type Value = Vec<u8>;

/// The error type for prolly tree operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A referenced node is not in the [`NodeStore`].
    #[error("node {0} not found")]
    Missing(NodeId),
    /// A node could not be decoded, or a child reference is malformed.
    #[error("node {0} is corrupt")]
    Corrupt(NodeId),
    /// Entries passed to [`Prolly::build`] are not strictly sorted by key.
    #[error("entries are not strictly sorted by key")]
    Unsorted,
    /// The [`NodeStore`] backend failed.
    #[error("node store: {0}")]
    Store(String),
}

/// A specialized [`Result`](std::result::Result) type whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
