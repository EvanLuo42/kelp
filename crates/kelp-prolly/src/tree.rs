//! Building, querying and editing trees.

use std::collections::BTreeMap;

use crate::chunker::LevelChunker;
use crate::cursor::Cursor;
use crate::node::child_id;
use crate::{ChunkConfig, Error, Key, Node, NodeId, NodeStore, Result, Value};

type Edit = (Key, Option<Value>);

/// A handle for operating on prolly trees stored in `S`.
///
/// Trees are identified by their root [`NodeId`] and are immutable; edits
/// return a new root that shares unchanged nodes with the old one. Roots are
/// only comparable between handles that use the same [`ChunkConfig`].
pub struct Prolly<'s, S: ?Sized> {
    store: &'s S,
    config: ChunkConfig,
}

impl<'s, S: NodeStore + ?Sized> Prolly<'s, S> {
    /// Creates a handle over `store` that chunks nodes with `config`.
    pub fn new(store: &'s S, config: ChunkConfig) -> Self {
        Self { store, config }
    }

    pub(crate) fn store(&self) -> &'s S {
        self.store
    }

    /// Stores the empty tree and returns its root.
    ///
    /// # Errors
    ///
    /// Returns an error if the store fails to write the node.
    pub fn empty(&self) -> Result<NodeId> {
        self.store.put(Node::empty_leaf())
    }

    /// Builds a tree from entries strictly sorted by key and returns its root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsorted`] if the keys are not strictly increasing, or
    /// an error from the store if a node cannot be written.
    pub fn build(&self, entries: impl IntoIterator<Item = (Key, Value)>) -> Result<NodeId> {
        let entries: Vec<_> = entries.into_iter().collect();
        if entries.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(Error::Unsorted);
        }
        self.build_up(entries, 0)
    }

    /// Returns the value stored under `key` in the tree `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if a node on the path to `key` cannot be loaded.
    pub fn get(&self, root: NodeId, key: &[u8]) -> Result<Option<Value>> {
        let mut node = self.store.get(&root)?;
        while node.level > 0 {
            node = self.store.get(&node.child_id(node.child_index(key))?)?;
        }
        Ok(node
            .entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .ok()
            .map(|i| node.entries[i].1.clone()))
    }

    /// Visits the nodes of the tree at `root` depth-first.
    ///
    /// `visit` receives each node's id and the node, and returns whether to
    /// descend into its children, so callers can skip subtrees they have
    /// already seen.
    ///
    /// # Errors
    ///
    /// Returns an error if a node cannot be read.
    pub fn walk(&self, root: NodeId, visit: &mut dyn FnMut(NodeId, &Node) -> bool) -> Result<()> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let node = self.store.get(&id)?;
            if visit(id, &node) && node.level > 0 {
                for i in 0..node.entries.len() {
                    stack.push(node.child_id(i)?);
                }
            }
        }
        Ok(())
    }

    /// Returns all entries of the tree `root` in key order.
    ///
    /// # Errors
    ///
    /// Returns an error if any node of the tree cannot be loaded.
    pub fn entries(&self, root: NodeId) -> Result<Vec<(Key, Value)>> {
        let mut out = Vec::new();
        self.collect(root, &mut out)?;
        Ok(out)
    }

    fn collect(&self, id: NodeId, out: &mut Vec<(Key, Value)>) -> Result<()> {
        let node = self.store.get(&id)?;
        if node.level == 0 {
            out.extend(node.entries.iter().cloned());
        } else {
            for i in 0..node.entries.len() {
                self.collect(node.child_id(i)?, out)?;
            }
        }
        Ok(())
    }

    /// Applies edits to the tree `root` and returns the new root.
    ///
    /// Each edit is an upsert (`Some`) or a deletion (`None`); later edits to
    /// the same key win. On each level only the nodes overlapping an edit, plus
    /// the few after them that the chunker needs to resynchronize, are
    /// rewritten. The result is identical to building the edited entry set
    /// from scratch.
    ///
    /// # Errors
    ///
    /// Returns an error if a node cannot be loaded from or written to the store.
    pub fn apply(&self, root: NodeId, edits: impl IntoIterator<Item = Edit>) -> Result<NodeId> {
        let mut edits: Vec<Edit> = edits
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        if edits.is_empty() {
            return Ok(root);
        }
        let root_node = self.store.get(&root)?;
        let top = root_node.level;
        for level in 0..=top {
            let parent_edits = self.rechunk_level(&root_node, level, &edits)?;
            if level == top {
                // The old root was the only node on this level, so the parent
                // edits hold every node of the new level.
                let nodes = parent_edits
                    .into_iter()
                    .filter_map(|(k, v)| Some((k, v?)))
                    .collect();
                return self.build_up(nodes, level + 1);
            }
            edits = parent_edits.into_iter().collect();
        }
        unreachable!("loop returns at the top level")
    }

    /// Re-chunks the nodes of `level` touched by `edits`.
    ///
    /// Returns the resulting edits for the level above: the first keys of
    /// replaced nodes map to `None`, and those of new nodes to their ids.
    fn rechunk_level(
        &self,
        root: &std::sync::Arc<Node>,
        level: u8,
        edits: &[Edit],
    ) -> Result<BTreeMap<Key, Option<Value>>> {
        let mut parent = BTreeMap::new();
        let mut ei = 0;
        while ei < edits.len() {
            let mut cursor = Cursor::seek(self.store, root.clone(), level, &edits[ei].0)?;
            let mut chunker = LevelChunker::new(self.store, self.config, level);
            loop {
                let next_key = cursor.next_first_key();
                if let Some((first, _)) = cursor.current.entries.first() {
                    parent.insert(first.clone(), None);
                }
                let end = match &next_key {
                    Some(nk) => ei + edits[ei..].partition_point(|(k, _)| k < nk),
                    None => edits.len(),
                };
                merge(&cursor.current.entries, &edits[ei..end], |k, v| {
                    chunker.push(k, v)
                })?;
                ei = end;
                if chunker.at_boundary() {
                    // Boundaries now line up with the old tree again.
                    break;
                }
                if next_key.is_none() || !cursor.advance()? {
                    chunker.flush()?;
                    break;
                }
            }
            for (first, id) in chunker.out {
                parent.insert(first, Some(id.0.to_vec()));
            }
        }
        Ok(parent)
    }

    /// Builds levels upward from the entries of `level` until one node remains.
    fn build_up(&self, mut entries: Vec<(Key, Value)>, mut level: u8) -> Result<NodeId> {
        loop {
            if entries.is_empty() {
                return self.empty();
            }
            if level > 0 && entries.len() == 1 {
                return self.collapse(child_id(&entries[0].1)?);
            }
            let mut chunker = LevelChunker::new(self.store, self.config, level);
            for (k, v) in entries {
                chunker.push(k, v)?;
            }
            chunker.flush()?;
            if chunker.out.len() == 1 {
                return Ok(chunker.out[0].1);
            }
            entries = chunker
                .out
                .into_iter()
                .map(|(k, id)| (k, id.0.to_vec()))
                .collect();
            level += 1;
        }
    }

    /// Descends through single-child internal nodes, since a canonical root
    /// never has a single child.
    fn collapse(&self, mut id: NodeId) -> Result<NodeId> {
        loop {
            let node = self.store.get(&id)?;
            if node.level == 0 || node.entries.len() != 1 {
                return Ok(id);
            }
            id = node.child_id(0)?;
        }
    }
}

fn merge(
    old: &[(Key, Value)],
    edits: &[Edit],
    mut emit: impl FnMut(Key, Value) -> Result<()>,
) -> Result<()> {
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < edits.len() {
        let take_old = j == edits.len() || (i < old.len() && old[i].0 < edits[j].0);
        if take_old {
            emit(old[i].0.clone(), old[i].1.clone())?;
            i += 1;
            continue;
        }
        if i < old.len() && old[i].0 == edits[j].0 {
            i += 1;
        }
        if let Some(v) = &edits[j].1 {
            emit(edits[j].0.clone(), v.clone())?;
        }
        j += 1;
    }
    Ok(())
}
