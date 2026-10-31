use std::sync::Arc;

use crate::{Key, Node, NodeStore, Result};

/// Walks the nodes of one level left to right.
pub(crate) struct Cursor<'a, S: ?Sized> {
    store: &'a S,
    level: u8,
    /// Ancestors from the root down to `level + 1`, with the index of the
    /// entry pointing at the next node down.
    stack: Vec<(Arc<Node>, usize)>,
    pub current: Arc<Node>,
}

impl<'a, S: NodeStore + ?Sized> Cursor<'a, S> {
    /// Positions a cursor at the node of `level` whose key range contains `key`.
    pub fn seek(store: &'a S, root: Arc<Node>, level: u8, key: &[u8]) -> Result<Self> {
        let mut stack = Vec::new();
        let mut node = root;
        while node.level > level {
            let i = node.child_index(key);
            let child = store.get(&node.child_id(i)?)?;
            stack.push((node, i));
            node = child;
        }
        Ok(Self {
            store,
            level,
            stack,
            current: node,
        })
    }

    /// Returns the first key of the following node on this level, if any.
    pub fn next_first_key(&self) -> Option<Key> {
        self.stack
            .iter()
            .rev()
            .find(|(n, i)| i + 1 < n.entries.len())
            .map(|(n, i)| n.entries[i + 1].0.clone())
    }

    /// Moves to the following node on this level.
    ///
    /// Returns `false` if the cursor was already at the last node.
    pub fn advance(&mut self) -> Result<bool> {
        while let Some((node, i)) = self.stack.pop() {
            if i + 1 < node.entries.len() {
                self.stack.push((node, i + 1));
                loop {
                    let (parent, i) = self.stack.last().expect("just pushed");
                    let child = self.store.get(&parent.child_id(*i)?)?;
                    if child.level == self.level {
                        self.current = child;
                        return Ok(true);
                    }
                    self.stack.push((child, 0));
                }
            }
        }
        Ok(false)
    }
}
