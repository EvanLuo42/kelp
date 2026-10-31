//! The hierarchical Merkle file tree.
//!
//! Each directory is a prolly tree keyed by case-folded entry name, so small
//! directories are a single node while huge ones, with tens of thousands of
//! entries, split into bounded, history-independent nodes. A directory's
//! [`TreeId`] is its prolly root, which makes subtree comparison, branching
//! and grafting of imported streams O(1).

use std::collections::{BTreeMap, HashSet};

use kelp_core::{ContentId, TreeId};
use kelp_prolly::{ChunkConfig, NodeStore, Prolly};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The kind and payload of a directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// A regular file.
    File {
        /// The hash of the file content.
        content: ContentId,
        /// The file size in bytes.
        size: u64,
        /// Whether the executable bit is set.
        executable: bool,
    },
    /// A subdirectory.
    Dir {
        /// The root of the subdirectory's tree.
        tree: TreeId,
    },
    /// A symbolic link.
    Symlink {
        /// The link target, stored verbatim.
        target: String,
    },
}

/// A named entry in a directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// The name as originally spelled.
    pub name: String,
    /// What the entry refers to.
    pub kind: EntryKind,
}

impl TreeEntry {
    /// Returns the case-folded name used for ordering and uniqueness.
    pub fn key(&self) -> String {
        fold(&self.name)
    }

    fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("tree entry serialization is infallible")
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).map_err(|_| Error::CorruptEntry)
    }
}

pub use kelp_core::path::fold;

/// A file-level difference between two trees.
///
/// Directories never appear as `old` or `new`; their contents are reported
/// file by file instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathDiff {
    /// The stream-relative path, spelled as in the newer tree when present.
    pub path: String,
    /// The entry before the change, or `None` if it was added.
    pub old: Option<EntryKind>,
    /// The entry after the change, or `None` if it was removed.
    pub new: Option<EntryKind>,
}

type PathChanges = Vec<(Vec<String>, Option<EntryKind>)>;

/// Directory operations over a [`NodeStore`].
pub struct Dirs<'s, S: ?Sized> {
    prolly: Prolly<'s, S>,
}

impl<'s, S: NodeStore + ?Sized> Dirs<'s, S> {
    /// Adds the ids of every node of the tree at `root`, including its
    /// subdirectories, to `seen`, and the content of every file to
    /// `contents`. Subtrees already in `seen` are skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if a node cannot be loaded or decoded.
    pub fn collect(
        &self,
        root: TreeId,
        seen: &mut HashSet<kelp_prolly::NodeId>,
        contents: &mut HashSet<ContentId>,
    ) -> Result<()> {
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            let mut failed = None;
            self.prolly.walk(dir.0, &mut |id, node| {
                if !seen.insert(id) {
                    return false;
                }
                if node.level == 0 {
                    for (_, value) in &node.entries {
                        match TreeEntry::decode(value) {
                            Ok(TreeEntry {
                                kind: EntryKind::Dir { tree },
                                ..
                            }) => dirs.push(tree),
                            Ok(TreeEntry {
                                kind: EntryKind::File { content, .. },
                                ..
                            }) => {
                                contents.insert(content);
                            }
                            Ok(_) => {}
                            Err(e) => failed = Some(e),
                        }
                    }
                }
                true
            })?;
            if let Some(e) = failed {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Removes the file or directory at the stream-relative `path`, and any
    /// directories left empty. Removing a missing path changes nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if a node cannot be loaded, decoded or written.
    pub fn remove_path(&self, root: TreeId, path: &str) -> Result<TreeId> {
        let segments: Vec<&str> = path.split('/').collect();
        self.remove_in(root, &segments).map(|t| t.unwrap_or(root))
    }

    /// Returns the new tree, or `None` if nothing was removed.
    fn remove_in(&self, dir: TreeId, segments: &[&str]) -> Result<Option<TreeId>> {
        let Some((first, rest)) = segments.split_first() else {
            return Ok(None);
        };
        let Some(entry) = self.get(dir, first)? else {
            return Ok(None);
        };
        let replacement = match (rest.is_empty(), &entry.kind) {
            (true, _) => None,
            (false, EntryKind::Dir { tree }) => match self.remove_in(*tree, rest)? {
                None => return Ok(None),
                Some(child) if child == self.empty()? => None,
                Some(child) => Some(TreeEntry {
                    name: entry.name.clone(),
                    kind: EntryKind::Dir { tree: child },
                }),
            },
            (false, _) => return Ok(None),
        };
        Ok(Some(self.update(dir, vec![(entry.name, replacement)])?))
    }

    /// Creates a handle over `store`.
    pub fn new(store: &'s S) -> Self {
        Self {
            prolly: Prolly::new(store, ChunkConfig::DEFAULT),
        }
    }

    /// Stores the empty directory and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Prolly`] if the node cannot be written.
    pub fn empty(&self) -> Result<TreeId> {
        Ok(TreeId(self.prolly.empty()?))
    }

    /// Builds a directory from `entries`, in any order, and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateEntry`] if two names are equal after case
    /// folding, or [`Error::Prolly`] if a node cannot be written.
    pub fn build(&self, entries: Vec<TreeEntry>) -> Result<TreeId> {
        let mut keyed: Vec<_> = entries
            .into_iter()
            .map(|e| (e.key().into_bytes(), e))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(w) = keyed.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(Error::DuplicateEntry(w[1].1.name.clone()));
        }
        let root = self
            .prolly
            .build(keyed.into_iter().map(|(k, e)| (k, e.encode())))?;
        Ok(TreeId(root))
    }

    /// Returns the entry of `dir` whose name matches `name` after case folding.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Prolly`] if a node cannot be loaded, or
    /// [`Error::CorruptEntry`] if the entry cannot be decoded.
    pub fn get(&self, dir: TreeId, name: &str) -> Result<Option<TreeEntry>> {
        self.prolly
            .get(dir.0, fold(name).as_bytes())?
            .map(|v| TreeEntry::decode(&v))
            .transpose()
    }

    /// Returns all entries of `dir`, ordered by folded name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Prolly`] if a node cannot be loaded, or
    /// [`Error::CorruptEntry`] if an entry cannot be decoded.
    pub fn list(&self, dir: TreeId) -> Result<Vec<TreeEntry>> {
        self.prolly
            .entries(dir.0)?
            .iter()
            .map(|(_, v)| TreeEntry::decode(v))
            .collect()
    }

    /// Inserts or replaces (`Some`) and removes (`None`) entries of `dir` by name,
    /// returning the new directory id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Prolly`] if a node cannot be loaded or written.
    pub fn update(&self, dir: TreeId, edits: Vec<(String, Option<TreeEntry>)>) -> Result<TreeId> {
        let edits = edits
            .into_iter()
            .map(|(name, e)| (fold(&name).into_bytes(), e.map(|e| e.encode())));
        Ok(TreeId(self.prolly.apply(dir.0, edits)?))
    }

    /// Returns the entry at a stream-relative path such as `Content/Hero.png`.
    ///
    /// Returns `Ok(None)` if any segment is missing or an intermediate segment
    /// is not a directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Prolly`] if a node cannot be loaded, or
    /// [`Error::CorruptEntry`] if an entry cannot be decoded.
    pub fn lookup(&self, root: TreeId, path: &str) -> Result<Option<TreeEntry>> {
        let mut dir = root;
        let mut segments = path.split('/').peekable();
        while let Some(name) = segments.next() {
            let Some(entry) = self.get(dir, name)? else {
                return Ok(None);
            };
            if segments.peek().is_none() {
                return Ok(Some(entry));
            }
            match entry.kind {
                EntryKind::Dir { tree } => dir = tree,
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Sets (`Some`) or removes (`None`) files at stream-relative paths and
    /// returns the new root.
    ///
    /// Missing directories are created, and directories left empty are removed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PathConflict`] if a path would be both a file and a
    /// directory, or an error if a node cannot be loaded, decoded or written.
    pub fn update_paths(
        &self,
        root: TreeId,
        changes: Vec<(String, Option<EntryKind>)>,
    ) -> Result<TreeId> {
        let changes = changes
            .into_iter()
            .map(|(p, k)| (p.split('/').map(str::to_owned).collect(), k))
            .collect();
        self.update_dir(root, changes)
    }

    fn update_dir(&self, dir: TreeId, changes: PathChanges) -> Result<TreeId> {
        let mut direct = Vec::new();
        let mut nested: BTreeMap<String, (String, PathChanges)> = BTreeMap::new();
        for (mut segments, kind) in changes {
            if segments.len() == 1 {
                direct.push((segments.pop().expect("one segment"), kind));
            } else {
                let rest = segments.split_off(1);
                let name = segments.pop().expect("first segment");
                nested
                    .entry(fold(&name))
                    .or_insert_with(|| (name, Vec::new()))
                    .1
                    .push((rest, kind));
            }
        }

        // A file may become a directory, or a directory a file, in one
        // change: the old entry is removed, or emptied, at the same time.
        let mut direct: BTreeMap<String, (String, Option<EntryKind>)> = direct
            .into_iter()
            .map(|(name, kind)| (fold(&name), (name, kind)))
            .collect();
        let mut edits = Vec::new();
        let empty = self.empty()?;
        for (key, (name, changes)) in nested {
            let replaced = direct.remove(&key);
            let (child, name) = match self.get(dir, &name)? {
                Some(TreeEntry {
                    kind: EntryKind::Dir { tree },
                    name,
                }) => (tree, name),
                Some(_) if matches!(replaced, Some((_, None))) => (empty, name),
                Some(_) => return Err(Error::PathConflict(name)),
                None => (empty, name),
            };
            let updated = self.update_dir(child, changes)?;
            match replaced {
                Some((file, Some(kind))) => {
                    if updated != empty {
                        return Err(Error::PathConflict(file));
                    }
                    edits.push((file.clone(), Some(TreeEntry { name: file, kind })));
                }
                _ => {
                    let entry = (updated != empty).then(|| TreeEntry {
                        name: name.clone(),
                        kind: EntryKind::Dir { tree: updated },
                    });
                    edits.push((name, entry));
                }
            }
        }
        for (name, kind) in direct.into_values() {
            if let Some(TreeEntry {
                kind: EntryKind::Dir { .. },
                ..
            }) = self.get(dir, &name)?
            {
                return Err(Error::PathConflict(name));
            }
            edits.push((name.clone(), kind.map(|kind| TreeEntry { name, kind })));
        }
        self.update(dir, edits)
    }

    /// Returns the file-level differences from `from` to `to`.
    ///
    /// A `from` of `None` is treated as the empty tree. Unchanged subtrees are
    /// skipped without being read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Prolly`] if a node cannot be loaded, or
    /// [`Error::CorruptEntry`] if an entry cannot be decoded.
    pub fn diff_paths(&self, from: Option<TreeId>, to: TreeId) -> Result<Vec<PathDiff>> {
        let mut out = Vec::new();
        let empty = self.empty()?;
        self.diff_dir("", from.unwrap_or(empty), to, empty, &mut out)?;
        Ok(out)
    }

    fn diff_dir(
        &self,
        prefix: &str,
        a: TreeId,
        b: TreeId,
        empty: TreeId,
        out: &mut Vec<PathDiff>,
    ) -> Result<()> {
        if a == b {
            return Ok(());
        }
        for d in self.prolly.diff(a.0, b.0)? {
            let (old, new) = match d {
                kelp_prolly::Diff::Added(_, v) => (None, Some(TreeEntry::decode(&v)?)),
                kelp_prolly::Diff::Removed(_, v) => (Some(TreeEntry::decode(&v)?), None),
                kelp_prolly::Diff::Modified(_, x, y) => {
                    (Some(TreeEntry::decode(&x)?), Some(TreeEntry::decode(&y)?))
                }
            };
            let name = new
                .as_ref()
                .or(old.as_ref())
                .map(|e| e.name.clone())
                .expect("one side exists");
            let path = format!("{prefix}{name}");
            let sub = |e: &Option<TreeEntry>| match e {
                Some(TreeEntry {
                    kind: EntryKind::Dir { tree },
                    ..
                }) => Some(*tree),
                _ => None,
            };
            let file = |e: Option<TreeEntry>| {
                e.map(|e| e.kind)
                    .filter(|k| !matches!(k, EntryKind::Dir { .. }))
            };
            let (sub_a, sub_b) = (sub(&old), sub(&new));
            if sub_a.is_some() || sub_b.is_some() {
                self.diff_dir(
                    &format!("{path}/"),
                    sub_a.unwrap_or(empty),
                    sub_b.unwrap_or(empty),
                    empty,
                    out,
                )?;
            }
            let (old, new) = (file(old), file(new));
            if old.is_some() || new.is_some() {
                out.push(PathDiff { path, old, new });
            }
        }
        Ok(())
    }
}
