//! Tree nodes and their canonical encoding.

use serde::{Deserialize, Serialize};

use crate::{Error, Key, Result, Value};

/// The hash of a node's canonical encoding.
pub type NodeId = kelp_core::Hash;

/// A tree node.
///
/// Leaves, at level 0, hold user entries. Internal nodes map the first key of
/// each child to the child's [`NodeId`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The height of the node above the leaves.
    pub level: u8,
    /// The entries, strictly sorted by key.
    pub entries: Vec<(Key, Value)>,
}

impl Node {
    /// Creates a leaf with no entries, the root of an empty tree.
    pub fn empty_leaf() -> Self {
        Self {
            level: 0,
            entries: Vec::new(),
        }
    }

    /// Returns the canonical serialized form of the node.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("node serialization is infallible")
    }

    /// Decodes a node previously produced by [`encode`](Self::encode).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corrupt`] with `id` if `bytes` is not a valid encoding.
    pub fn decode(id: NodeId, bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).map_err(|_| Error::Corrupt(id))
    }

    /// Returns the id of the node, the hash of its canonical encoding.
    pub fn id(&self) -> NodeId {
        NodeId::of(&self.encode())
    }

    /// Returns the index of the child whose range contains `key`.
    ///
    /// Only meaningful for internal nodes.
    pub(crate) fn child_index(&self, key: &[u8]) -> usize {
        self.entries
            .partition_point(|(k, _)| k.as_slice() <= key)
            .saturating_sub(1)
    }

    pub(crate) fn child_id(&self, i: usize) -> Result<NodeId> {
        child_id(&self.entries[i].1)
    }
}

pub(crate) fn child_id(value: &[u8]) -> Result<NodeId> {
    let bytes: [u8; 32] = value
        .try_into()
        .map_err(|_| Error::Corrupt(NodeId::of(value)))?;
    Ok(kelp_core::Hash(bytes))
}
