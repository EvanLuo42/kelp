use std::collections::VecDeque;

use crate::{Key, NodeId, NodeStore, Prolly, Result, Value};

/// A single difference between two trees, as produced by [`Prolly::diff`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diff {
    /// The entry exists only in the new tree.
    Added(Key, Value),
    /// The entry exists only in the old tree.
    Removed(Key, Value),
    /// The key exists in both trees with different values, old value first.
    Modified(Key, Value, Value),
}

impl Diff {
    /// Returns the key this difference applies to.
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Added(k, _) | Self::Removed(k, _) | Self::Modified(k, ..) => k,
        }
    }
}

enum Item {
    Entry(Key, Value),
    /// A subtree whose entries are all `>= first`.
    Tree {
        first: Key,
        level: u8,
        id: NodeId,
    },
}

impl<S: NodeStore + ?Sized> Prolly<'_, S> {
    /// Returns the changes from tree `a` to tree `b`, in key order.
    ///
    /// Subtrees with identical ids are skipped, so the cost scales with the
    /// size of the change rather than the size of the trees.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Missing`](crate::Error::Missing) or
    /// [`Error::Corrupt`](crate::Error::Corrupt) if a node that must be visited
    /// cannot be loaded.
    pub fn diff(&self, a: NodeId, b: NodeId) -> Result<Vec<Diff>> {
        let store = self.store();
        let root = |id: NodeId| -> Result<VecDeque<Item>> {
            let level = store.get(&id)?.level;
            Ok(VecDeque::from([Item::Tree {
                first: Vec::new(),
                level,
                id,
            }]))
        };
        let (mut qa, mut qb) = (root(a)?, root(b)?);
        let mut out = Vec::new();
        loop {
            match (qa.front(), qb.front()) {
                (None, None) => return Ok(out),
                (Some(Item::Tree { id: x, .. }), Some(Item::Tree { id: y, .. })) if x == y => {
                    qa.pop_front();
                    qb.pop_front();
                }
                (Some(Item::Entry(ka, _)), Some(Item::Entry(kb, _))) => {
                    let ord = ka.cmp(kb);
                    let ea = if ord.is_le() { qa.pop_front() } else { None };
                    let eb = if ord.is_ge() { qb.pop_front() } else { None };
                    match (ea, eb) {
                        (Some(Item::Entry(k, va)), Some(Item::Entry(_, vb))) => {
                            if va != vb {
                                out.push(Diff::Modified(k, va, vb));
                            }
                        }
                        (Some(Item::Entry(k, v)), None) => out.push(Diff::Removed(k, v)),
                        (None, Some(Item::Entry(k, v))) => out.push(Diff::Added(k, v)),
                        _ => unreachable!("fronts are entries"),
                    }
                }
                (Some(Item::Entry(k, _)), Some(Item::Tree { first, .. })) if k < first => {
                    let Some(Item::Entry(k, v)) = qa.pop_front() else {
                        unreachable!()
                    };
                    out.push(Diff::Removed(k, v));
                }
                (Some(Item::Tree { first, .. }), Some(Item::Entry(k, _))) if k < first => {
                    let Some(Item::Entry(k, v)) = qb.pop_front() else {
                        unreachable!()
                    };
                    out.push(Diff::Added(k, v));
                }
                (None, Some(Item::Entry(..))) => {
                    let Some(Item::Entry(k, v)) = qb.pop_front() else {
                        unreachable!()
                    };
                    out.push(Diff::Added(k, v));
                }
                (Some(Item::Entry(..)), None) => {
                    let Some(Item::Entry(k, v)) = qa.pop_front() else {
                        unreachable!()
                    };
                    out.push(Diff::Removed(k, v));
                }
                (fa, fb) => {
                    // Expand the subtree that starts earlier, or the higher one.
                    let rank = |i: Option<&Item>| match i {
                        Some(Item::Tree { first, level, .. }) => Some((first.clone(), *level)),
                        _ => None,
                    };
                    let (ra, rb) = (rank(fa), rank(fb));
                    let expand_a = match (&ra, &rb) {
                        (Some(_), None) => true,
                        (None, _) => false,
                        (Some((fa, la)), Some((fb, lb))) => fa < fb || (fa == fb && la >= lb),
                    };
                    let expand_b = match (&ra, &rb) {
                        (None, Some(_)) => true,
                        (_, None) => false,
                        (Some((fa, la)), Some((fb, lb))) => fb < fa || (fa == fb && lb >= la),
                    };
                    if expand_a {
                        self.expand(&mut qa)?;
                    }
                    if expand_b {
                        self.expand(&mut qb)?;
                    }
                }
            }
        }
    }

    fn expand(&self, q: &mut VecDeque<Item>) -> Result<()> {
        let Some(Item::Tree { id, .. }) = q.pop_front() else {
            unreachable!("front is a tree")
        };
        let node = self.store().get(&id)?;
        for (i, (k, v)) in node.entries.iter().enumerate().rev() {
            q.push_front(if node.level == 0 {
                Item::Entry(k.clone(), v.clone())
            } else {
                Item::Tree {
                    first: k.clone(),
                    level: node.level - 1,
                    id: node.child_id(i)?,
                }
            });
        }
        Ok(())
    }
}
