//! Node persistence.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{Error, Node, NodeId, Result};

/// Persistent storage for prolly nodes.
///
/// The metadata database and client caches implement this trait; [`MemStore`]
/// serves tests and transient trees.
pub trait NodeStore {
    /// Returns the node with the given id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Missing`] if the node is absent, [`Error::Corrupt`] if
    /// it cannot be decoded, and [`Error::Store`] on backend failures.
    fn get(&self, id: &NodeId) -> Result<Arc<Node>>;

    /// Stores `node` and returns its id.
    ///
    /// Storing a node that already exists is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on backend failures.
    fn put(&self, node: Node) -> Result<NodeId>;
}

/// An in-memory [`NodeStore`].
#[derive(Debug, Default)]
pub struct MemStore {
    nodes: Mutex<HashMap<NodeId, Arc<Node>>>,
}

impl MemStore {
    /// Returns the number of distinct nodes stored.
    pub fn len(&self) -> usize {
        self.nodes.lock().unwrap().len()
    }

    /// Returns `true` if no nodes are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl NodeStore for MemStore {
    fn get(&self, id: &NodeId) -> Result<Arc<Node>> {
        self.nodes
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or(Error::Missing(*id))
    }

    fn put(&self, node: Node) -> Result<NodeId> {
        let id = node.id();
        self.nodes
            .lock()
            .unwrap()
            .entry(id)
            .or_insert_with(|| Arc::new(node));
        Ok(id)
    }
}
