//! The migration to format 10, which folds names one character at a time
//! (see [`kelp_core::path::fold`]) where earlier formats lowercased whole
//! strings and so gave a word-final sigma, and a few other letter variants,
//! keys of their own.
//!
//! Tree entries and the tables keyed by folded paths are re-keyed. Records
//! that contain paths need no rewriting: a [`DepotPath`] recomputes its key
//! when it is decoded.
//!
//! Two names that only now fold alike would collide. Neither is lost: the
//! one already spelled like its new key, or else the first, takes the new
//! key, and the other keeps its old key, with a warning. Such a name is no
//! longer found by lookups, which fold the name they look for, but it stays
//! in listings and history so an administrator can rename it.
//!
//! [`DepotPath`]: kelp_core::DepotPath

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kelp_core::TreeId;
use kelp_core::path::fold;
use kelp_prolly::{ChunkConfig, Node, Prolly};
use redb::{ReadableTable, WriteTransaction};

use super::nodes::TxnNodes;
use super::{
    ARTIFACT_INDEX, ARTIFACT_RETENTION, ARTIFACTS, CHANGES, IMPORTERS, IMPORTS, INTEGRATIONS,
    LOCKS, MOVES, NODES, REVISIONS, STREAM_CHANGES, STREAM_ROOTS, STREAMS, VIEW_IMPORTS, VIEWS,
    dec, enc,
};
use crate::record::{Artifact, Changelist, Stream};
use crate::{EntryKind, Error, LockDomain, Result, TreeEntry};

pub(super) fn refold(txn: &WriteTransaction) -> Result<()> {
    refold_trees(txn)?;
    refold_keys(txn)
}

/// Re-keys the entries of every tree something refers to, if any entry is
/// keyed differently from how its name now folds.
fn refold_trees(txn: &WriteTransaction) -> Result<()> {
    if !trees_need_refolding(txn)? {
        return Ok(());
    }
    let nodes = TxnNodes::new(txn)?;
    let prolly = Prolly::new(&nodes, ChunkConfig::DEFAULT);
    let mut done = HashMap::new();
    let mut rewrite = |tree: TreeId| rewrite_tree(&prolly, tree, &mut done);

    let streams: Vec<Stream> = txn
        .open_table(STREAMS)?
        .iter()?
        .map(|r| dec("streams", r?.1.value()))
        .collect::<Result<_>>()?;
    for mut stream in streams {
        let (root, base_root) = (rewrite(stream.root)?, rewrite(stream.base_root)?);
        if (root, base_root) != (stream.root, stream.base_root) {
            (stream.root, stream.base_root) = (root, base_root);
            txn.open_table(STREAMS)?
                .insert(stream.path.key(), enc(&stream).as_slice())?;
        }
    }
    let changes: Vec<Changelist> = txn
        .open_table(CHANGES)?
        .iter()?
        .map(|r| dec("changes", r?.1.value()))
        .collect::<Result<_>>()?;
    for mut change in changes {
        let root = rewrite(change.root)?;
        if root != change.root {
            change.root = root;
            txn.open_table(CHANGES)?
                .insert(change.number.0, enc(&change).as_slice())?;
        }
    }
    let artifacts: Vec<Artifact> = txn
        .open_table(ARTIFACTS)?
        .iter()?
        .map(|r| dec("artifacts", r?.1.value()))
        .collect::<Result<_>>()?;
    for mut artifact in artifacts {
        let root = rewrite(artifact.root)?;
        if root != artifact.root {
            artifact.root = root;
            txn.open_table(ARTIFACTS)?
                .insert(artifact.id, enc(&artifact).as_slice())?;
        }
    }
    for table in [STREAM_ROOTS, VIEWS] {
        let rows: Vec<((String, u64), TreeId)> = txn
            .open_table(table)?
            .iter()?
            .map(|r| {
                let (k, v) = r?;
                let (stream, cl) = k.value();
                Ok((
                    (stream.to_owned(), cl),
                    super::imports::decode_tree(v.value())?,
                ))
            })
            .collect::<Result<_>>()?;
        for ((stream, cl), tree) in rows {
            let new = rewrite(tree)?;
            if new != tree {
                txn.open_table(table)?
                    .insert((stream.as_str(), cl), new.0.0.as_slice())?;
            }
        }
    }
    Ok(())
}

/// Returns whether a stored directory entry is keyed differently from how
/// its name folds now.
fn trees_need_refolding(txn: &WriteTransaction) -> Result<bool> {
    for row in txn.open_table(NODES)?.iter()? {
        let (id, bytes) = row?;
        let id: [u8; 32] = id
            .value()
            .try_into()
            .map_err(|_| Error::CorruptRecord("nodes"))?;
        let node = Node::decode(kelp_core::Hash(id), bytes.value())?;
        if node.level != 0 {
            continue;
        }
        for (key, value) in &node.entries {
            let Ok(entry) = postcard::from_bytes::<TreeEntry>(value) else {
                continue;
            };
            if fold(&entry.name).as_bytes() != key.as_slice() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Returns `dir` with every entry, in it and below, keyed by its name's
/// current folding.
fn rewrite_tree(
    prolly: &Prolly<'_, TxnNodes<'_>>,
    dir: TreeId,
    done: &mut HashMap<TreeId, TreeId>,
) -> Result<TreeId> {
    if let Some(new) = done.get(&dir) {
        return Ok(*new);
    }
    let mut changed = false;
    let mut entries = Vec::new();
    for (old, value) in prolly.entries(dir.0)? {
        let mut entry: TreeEntry = postcard::from_bytes(&value).map_err(|_| Error::CorruptEntry)?;
        if let EntryKind::Dir { tree } = entry.kind {
            let new = rewrite_tree(prolly, tree, done)?;
            if new != tree {
                entry.kind = EntryKind::Dir { tree: new };
                changed = true;
            }
        }
        let new = fold(&entry.name).into_bytes();
        changed |= new != old;
        entries.push((old, new, entry));
    }
    let result = match changed {
        false => dir,
        true => {
            // Entries already keyed as they fold now go first, so they keep
            // their key if another entry would now take it.
            entries.sort_by(|a, b| (a.0 != a.1, &a.0).cmp(&(b.0 != b.1, &b.0)));
            let mut keyed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            for (old, new, entry) in entries {
                let key = match keyed.contains_key(&new) {
                    false => new,
                    true => {
                        tracing::warn!(
                            name = %entry.name,
                            "another entry of its directory now folds alike; it keeps its old key \
                             and should be renamed"
                        );
                        old
                    }
                };
                if keyed.insert(key, enc(&entry)).is_some() {
                    return Err(Error::DuplicateEntry(entry.name));
                }
            }
            TreeId(prolly.build(keyed)?)
        }
    };
    done.insert(dir, result);
    Ok(result)
}

/// Owned forms of table keys, to remove and insert rows while iterating.
trait OwnedKey {
    type Key<'a>
    where
        Self: 'a;
    fn key(&self) -> Self::Key<'_>;
}

impl OwnedKey for String {
    type Key<'a> = &'a str;
    fn key(&self) -> &str {
        self
    }
}

impl OwnedKey for (String, u64) {
    type Key<'a> = (&'a str, u64);
    fn key(&self) -> (&str, u64) {
        (&self.0, self.1)
    }
}

impl OwnedKey for (String, u32) {
    type Key<'a> = (&'a str, u32);
    fn key(&self) -> (&str, u32) {
        (&self.0, self.1)
    }
}

impl OwnedKey for (String, String) {
    type Key<'a> = (&'a str, &'a str);
    fn key(&self) -> (&str, &str) {
        (&self.0, &self.1)
    }
}

impl OwnedKey for (String, String, u64, u64) {
    type Key<'a> = (&'a str, &'a str, u64, u64);
    fn key(&self) -> (&str, &str, u64, u64) {
        (&self.0, &self.1, self.2, self.3)
    }
}

/// Moves the rows of `$table` whose key `$remap` changes, keeping their
/// values, which `$own` copies out and `$borrow` lends back.
macro_rules! rekey {
    ($txn:expr, $table:expr, $remap:expr, $own:expr, $borrow:expr) => {{
        let (remap, own, borrow) = ($remap, $own, $borrow);
        let mut table = $txn.open_table($table)?;
        let mut moved = Vec::new();
        for row in table.iter()? {
            let (k, v) = row?;
            if let Some((old, new)) = remap(k.value()) {
                moved.push((old, new, own(v.value())));
            }
        }
        for (old, _, _) in &moved {
            table.remove(OwnedKey::key(old))?;
        }
        for (_, new, value) in &moved {
            table.insert(OwnedKey::key(new), borrow(value))?;
        }
    }};
}

/// Re-keys the tables keyed by folded stream or file paths, and the lock
/// domains streams record.
fn refold_keys(txn: &WriteTransaction) -> Result<()> {
    let renames = Renames::decide(txn)?;
    let r = |k: &str| renames.get(k);
    let bytes = |v: &[u8]| v.to_vec();
    fn lend(v: &Vec<u8>) -> &[u8] {
        v
    }

    rekey!(
        txn,
        STREAMS,
        |k: &str| r(k).map(|n| (k.to_owned(), n)),
        bytes,
        lend
    );
    rekey!(
        txn,
        IMPORTS,
        |k: &str| r(k).map(|n| (k.to_owned(), n)),
        bytes,
        lend
    );
    for table in [STREAM_CHANGES] {
        rekey!(
            txn,
            table,
            |(k, n): (&str, u64)| r(k).map(|new| ((k.to_owned(), n), (new, n))),
            |_: ()| (),
            |_: &()| ()
        );
    }
    for table in [VIEWS, STREAM_ROOTS, VIEW_IMPORTS, MOVES] {
        rekey!(
            txn,
            table,
            |(k, n): (&str, u64)| r(k).map(|new| ((k.to_owned(), n), (new, n))),
            bytes,
            lend
        );
    }
    rekey!(
        txn,
        REVISIONS,
        |(k, n): (&str, u32)| r(k).map(|new| ((k.to_owned(), n), (new, n))),
        bytes,
        lend
    );
    let pair = |(a, b): (&str, &str)| match (r(a), r(b)) {
        (None, None) => None,
        (x, y) => Some((
            (a.to_owned(), b.to_owned()),
            (
                x.unwrap_or_else(|| a.to_owned()),
                y.unwrap_or_else(|| b.to_owned()),
            ),
        )),
    };
    rekey!(txn, INTEGRATIONS, pair, |v: u64| v, |v: &u64| *v);
    rekey!(txn, IMPORTERS, pair, |_: ()| (), |_: &()| ());
    rekey!(
        txn,
        ARTIFACT_RETENTION,
        |(k, name): (&str, &str)| r(k)
            .map(|n| ((k.to_owned(), name.to_owned()), (n, name.to_owned()))),
        |v: u32| v,
        |v: &u32| *v
    );
    rekey!(
        txn,
        ARTIFACT_INDEX,
        |(k, name, cl, id): (&str, &str, u64, u64)| r(k).map(|n| (
            (k.to_owned(), name.to_owned(), cl, id),
            (n, name.to_owned(), cl, id)
        )),
        |_: ()| (),
        |_: &()| ()
    );
    refold_locks(txn, &renames)?;

    let mut table = txn.open_table(STREAMS)?;
    let streams: Vec<(String, Stream)> = table
        .iter()?
        .map(|r| {
            let (k, v) = r?;
            Ok((k.value().to_owned(), dec("streams", v.value())?))
        })
        .collect::<Result<_>>()?;
    for (key, mut stream) in streams {
        if let Some(domain) = renames.get(&stream.lock_domain.0) {
            stream.lock_domain = LockDomain(domain);
            table.insert(key.as_str(), enc(&stream).as_slice())?;
        }
    }
    Ok(())
}

/// Re-keys locks by their renamed domain and refolded path. A lock whose
/// new key another lock already has keeps its old one.
fn refold_locks(txn: &WriteTransaction, renames: &Renames) -> Result<()> {
    let mut table = txn.open_table(LOCKS)?;
    let mut moved = Vec::new();
    let mut keys = BTreeSet::new();
    for row in table.iter()? {
        let (k, v) = row?;
        let (domain, path) = k.value();
        keys.insert((domain.to_owned(), path.to_owned()));
        let new = (
            renames.get(domain).unwrap_or_else(|| domain.to_owned()),
            fold(path),
        );
        if new.0 != domain || new.1 != path {
            moved.push((
                (domain.to_owned(), path.to_owned()),
                new,
                v.value().to_vec(),
            ));
        }
    }
    for (old, _, _) in &moved {
        keys.remove(old);
        table.remove(old.key())?;
    }
    for (old, new, value) in moved {
        let key = match keys.contains(&new) {
            false => new,
            true => {
                tracing::warn!(domain = %old.0, path = %old.1, "a lock now folds like another; it keeps its old key");
                old
            }
        };
        table.insert(key.key(), value.as_slice())?;
        keys.insert(key);
    }
    Ok(())
}

/// The new keys of folded stream and file paths that change.
struct Renames(HashMap<String, String>);

impl Renames {
    /// Decides the new key of every path key in the tables keyed by them.
    fn decide(txn: &WriteTransaction) -> Result<Self> {
        let mut keys: BTreeSet<String> = BTreeSet::new();
        macro_rules! collect {
            ($table:expr, $pick:expr) => {{
                let pick = $pick;
                for row in txn.open_table($table)?.iter()? {
                    let (k, _) = row?;
                    keys.extend(pick(k.value()));
                }
            }};
        }
        collect!(STREAMS, |k: &str| [k.to_owned()]);
        collect!(IMPORTS, |k: &str| [k.to_owned()]);
        for table in [VIEWS, STREAM_ROOTS, VIEW_IMPORTS, MOVES] {
            collect!(table, |(k, _): (&str, u64)| [k.to_owned()]);
        }
        collect!(STREAM_CHANGES, |(k, _): (&str, u64)| [k.to_owned()]);
        collect!(REVISIONS, |(k, _): (&str, u32)| [k.to_owned()]);
        collect!(INTEGRATIONS, |(a, b): (&str, &str)| [
            a.to_owned(),
            b.to_owned()
        ]);
        collect!(IMPORTERS, |(a, b): (&str, &str)| [
            a.to_owned(),
            b.to_owned()
        ]);
        collect!(ARTIFACT_RETENTION, |(k, _): (&str, &str)| [k.to_owned()]);
        collect!(ARTIFACT_INDEX, |(k, ..): (&str, &str, u64, u64)| [
            k.to_owned()
        ]);
        collect!(LOCKS, |(k, _): (&str, &str)| [k.to_owned()]);
        Ok(Self::from_keys(&keys))
    }

    fn from_keys(keys: &BTreeSet<String>) -> Self {
        let mut groups: BTreeMap<String, Vec<&String>> = BTreeMap::new();
        for key in keys {
            groups.entry(fold(key)).or_default().push(key);
        }
        let mut renames = HashMap::new();
        let mut kept: Vec<&String> = Vec::new();
        for (new, members) in groups {
            if members.iter().all(|k| **k == new) {
                continue;
            }
            // The key already spelled the new way, or else the first, wins.
            let winner = members
                .iter()
                .find(|k| ***k == new)
                .or(members.first())
                .copied()
                .expect("a group has a member");
            for key in members {
                if key == winner {
                    if *key != new {
                        renames.insert(key.clone(), new.clone());
                    }
                } else {
                    tracing::warn!(
                        path = %key,
                        "another path now folds alike; it keeps its old key and should be renamed"
                    );
                    kept.push(key);
                }
            }
        }
        // Below a path that keeps its key, paths keep theirs too, or they
        // would no longer lie below it.
        renames.retain(|key, _| {
            !kept.iter().any(|k| {
                key.len() > k.len()
                    && key.starts_with(k.as_str())
                    && key.as_bytes()[k.len()] == b'/'
            })
        });
        Self(renames)
    }

    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}
