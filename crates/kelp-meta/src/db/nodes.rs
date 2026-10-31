use std::cell::RefCell;
use std::sync::Arc;

use kelp_prolly::{Node, NodeId, NodeStore};
use redb::{ReadOnlyTable, ReadableTable, Table, WriteTransaction};

use super::{Bytes, NODES};
use crate::Result;

/// Prolly node storage inside a write transaction.
pub(super) struct TxnNodes<'t> {
    table: RefCell<Table<'t, Bytes, Bytes>>,
}

impl<'t> TxnNodes<'t> {
    pub(super) fn new(txn: &'t WriteTransaction) -> Result<Self> {
        Ok(Self {
            table: RefCell::new(txn.open_table(NODES)?),
        })
    }
}

fn store_err(e: impl std::fmt::Display) -> kelp_prolly::Error {
    kelp_prolly::Error::Store(e.to_string())
}

impl NodeStore for TxnNodes<'_> {
    fn get(&self, id: &NodeId) -> kelp_prolly::Result<Arc<Node>> {
        let table = self.table.borrow();
        let bytes = table
            .get(id.0.as_slice())
            .map_err(store_err)?
            .ok_or(kelp_prolly::Error::Missing(*id))?;
        Ok(Arc::new(Node::decode(*id, bytes.value())?))
    }

    fn put(&self, node: Node) -> kelp_prolly::Result<NodeId> {
        let id = node.id();
        self.table
            .borrow_mut()
            .insert(id.0.as_slice(), node.encode().as_slice())
            .map_err(store_err)?;
        Ok(id)
    }
}

/// A read-only [`NodeStore`] over a [`Reader`](super::Reader) snapshot.
///
/// [`put`](NodeStore::put) succeeds only for nodes that already exist and
/// returns [`kelp_prolly::Error::Store`] otherwise.
pub struct ReadNodes {
    table: ReadOnlyTable<Bytes, Bytes>,
}

impl ReadNodes {
    pub(super) fn new(table: ReadOnlyTable<Bytes, Bytes>) -> Self {
        Self { table }
    }
}

impl NodeStore for ReadNodes {
    fn get(&self, id: &NodeId) -> kelp_prolly::Result<Arc<Node>> {
        let bytes = self
            .table
            .get(id.0.as_slice())
            .map_err(store_err)?
            .ok_or(kelp_prolly::Error::Missing(*id))?;
        Ok(Arc::new(Node::decode(*id, bytes.value())?))
    }

    fn put(&self, node: Node) -> kelp_prolly::Result<NodeId> {
        let id = node.id();
        match self.table.get(id.0.as_slice()).map_err(store_err)? {
            Some(_) => Ok(id),
            None => Err(kelp_prolly::Error::Store(
                "cannot write to a read-only snapshot".into(),
            )),
        }
    }
}
