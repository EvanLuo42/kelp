//! Support for garbage collection: what is still referenced, and sweeping
//! what is not.

use std::collections::HashSet;

use kelp_core::{ClNumber, ContentId, Hash, TreeId};
use kelp_prolly::{Node, NodeId};
use redb::{ReadableTable, ReadableTableMetadata};

use super::nodes::TxnNodes;
use super::{
    ARTIFACTS, AUDIT, Bytes, CHANGES, LOG, MANIFESTS, MetaDb, NODES, REVISIONS, Reader, SHELVES,
    STREAM_ROOTS, STREAMS, VIEWS, audit_hash, dec,
};
use crate::record::{Artifact, AuditEntry, Changelist, Revision, Shelf, Stream};
use crate::{Dirs, Error, Result};

impl Reader {
    /// Returns the content that must be kept: every revision not purged,
    /// everything shelved or in a review, registered plugins, and everything
    /// in the trees streams were created with (whose source revisions may have been obliterated).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn live_contents(&self) -> Result<HashSet<ContentId>> {
        let mut live = HashSet::new();
        let revisions = self.txn().open_table(REVISIONS)?;
        for entry in revisions.iter()? {
            let rev: Revision = dec("revisions", entry?.1.value())?;
            if let (Some(content), false) = (rev.content, rev.purged) {
                live.insert(content);
            }
        }
        let shelves = self.txn().open_table(SHELVES)?;
        for entry in shelves.iter()? {
            let shelf: Shelf = dec("shelves", entry?.1.value())?;
            live.extend(
                shelf
                    .contents
                    .files
                    .iter()
                    .filter_map(|f| f.action.data())
                    .map(|d| d.content),
            );
        }
        live.extend(self.plugins()?.into_iter().map(|p| p.content));
        for review in self.reviews(None, false)? {
            live.extend(
                review
                    .versions
                    .iter()
                    .flat_map(|v| &v.files)
                    .filter_map(|f| f.action.data())
                    .map(|d| d.content),
            );
        }
        let nodes = self.nodes()?;
        let dirs = Dirs::new(&nodes);
        let mut seen = HashSet::new();
        for stream in self.streams()? {
            // The head is covered by its revisions too, but files a stream
            // currently has must never depend on retention being right.
            dirs.collect(stream.root, &mut seen, &mut live)?;
            dirs.collect(stream.base_root, &mut seen, &mut live)?;
        }
        for artifact in self.all_artifacts()? {
            dirs.collect(artifact.root, &mut seen, &mut live)?;
        }
        Ok(live)
    }

    /// Returns the content of every file in every stream's current tree.
    ///
    /// # Errors
    ///
    /// Returns an error if a table or node cannot be read.
    pub fn head_contents(&self) -> Result<HashSet<ContentId>> {
        let nodes = self.nodes()?;
        let dirs = Dirs::new(&nodes);
        let (mut seen, mut contents) = (HashSet::new(), HashSet::new());
        for stream in self.streams()? {
            dirs.collect(stream.root, &mut seen, &mut contents)?;
        }
        Ok(contents)
    }

    /// Returns the ids of every tree node reachable from a changelist or a
    /// stream, and the last changelist number considered.
    ///
    /// # Errors
    ///
    /// Returns an error if a table or node cannot be read.
    pub fn live_nodes(&self) -> Result<(HashSet<NodeId>, ClNumber)> {
        let txn = self.txn();
        let (roots, last) = tree_roots(
            &txn.open_table(CHANGES)?,
            &txn.open_table(STREAMS)?,
            &txn.open_table(ARTIFACTS)?,
            [&txn.open_table(STREAM_ROOTS)?, &txn.open_table(VIEWS)?],
            None,
        )?;
        let nodes = self.nodes()?;
        let dirs = Dirs::new(&nodes);
        let mut seen = HashSet::from([Node::empty_leaf().id()]);
        let mut contents = HashSet::new();
        for root in roots {
            dirs.collect(root, &mut seen, &mut contents)?;
        }
        Ok((seen, last))
    }

    /// Returns the content ids that have stored manifests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn manifest_ids(&self) -> Result<Vec<ContentId>> {
        let manifests = self.txn().open_table(MANIFESTS)?;
        manifests
            .iter()?
            .map(|r| {
                let key = r?.0;
                let bytes: [u8; 32] = key
                    .value()
                    .try_into()
                    .map_err(|_| Error::CorruptRecord("manifests"))?;
                Ok(ContentId(Hash(bytes)))
            })
            .collect()
    }

    /// Returns the number of stored tree nodes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn node_count(&self) -> Result<u64> {
        Ok(self.txn().open_table(NODES)?.len()?)
    }

    /// Returns up to `limit` audit entries starting at log index `from`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn audit(&self, from: u64, limit: usize) -> Result<Vec<AuditEntry>> {
        let audit = self.txn().open_table(AUDIT)?;
        audit
            .range(from..)?
            .take(limit)
            .map(|r| dec("audit", r?.1.value()))
            .collect()
    }

    /// Recomputes the audit chain from the command log and returns the number
    /// of entries and the hash of the last one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AuditMismatch`] at the first entry that does not match
    /// its command or predecessor.
    pub fn verify_audit(&self) -> Result<(u64, Hash)> {
        let log = self.txn().open_table(LOG)?;
        let audit = self.txn().open_table(AUDIT)?;
        let mut prev = Hash([0; 32]);
        let mut count = 0;
        for entry in log.iter()? {
            let (index, command) = entry?;
            let index = index.value();
            let recorded: AuditEntry = match audit.get(index)? {
                Some(v) => dec("audit", v.value())?,
                None => return Err(Error::AuditMismatch(index)),
            };
            let expected = audit_hash(&prev, index, &recorded.actor, command.value());
            if recorded.prev != prev || recorded.hash != expected {
                return Err(Error::AuditMismatch(index));
            }
            prev = recorded.hash;
            count += 1;
        }
        if audit.len()? != count {
            return Err(Error::AuditMismatch(count + 1));
        }
        Ok((count, prev))
    }
}

impl MetaDb {
    /// Deletes the manifests of `ids`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the transaction fails.
    pub fn delete_manifests(&self, ids: &[ContentId]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut manifests = txn.open_table(MANIFESTS)?;
            for id in ids {
                manifests.remove(id.0.0.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Deletes tree nodes that are neither in `live`, collected through
    /// changelist `marked_through`, nor reachable from a changelist submitted
    /// or a stream changed since. Returns the number deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if a node cannot be read or the transaction fails.
    pub fn sweep_nodes(&self, live: &HashSet<NodeId>, marked_through: ClNumber) -> Result<u64> {
        let txn = self.db.begin_write()?;
        let recent = {
            let (roots, _) = tree_roots(
                &txn.open_table(CHANGES)?,
                &txn.open_table(STREAMS)?,
                &txn.open_table(ARTIFACTS)?,
                [&txn.open_table(STREAM_ROOTS)?, &txn.open_table(VIEWS)?],
                Some(marked_through),
            )?;
            let nodes = TxnNodes::new(&txn)?;
            let dirs = Dirs::new(&nodes);
            let (mut recent, mut contents) = (HashSet::new(), HashSet::new());
            for root in roots {
                dirs.collect(root, &mut recent, &mut contents)?;
            }
            recent
        };
        let doomed: Vec<NodeId> = {
            let table = txn.open_table(NODES)?;
            table
                .iter()?
                .map(|r| {
                    let bytes: [u8; 32] =
                        r?.0.value()
                            .try_into()
                            .map_err(|_| Error::CorruptRecord("nodes"))?;
                    Ok(Hash(bytes))
                })
                .filter(|id| {
                    id.as_ref()
                        .map_or(true, |id| !live.contains(id) && !recent.contains(id))
                })
                .collect::<Result<_>>()?
        };
        {
            let mut table = txn.open_table(NODES)?;
            for id in &doomed {
                table.remove(id.0.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(doomed.len() as u64)
    }
}

/// Returns the trees that keep tree nodes alive, and the last changelist
/// number they cover: every stream's head and base tree, every artifact's,
/// and the trees of changelists and of the derived tables (a stream's tree
/// after another stream's changelist, and views).
///
/// With `marked_through`, only the changelists after it and the derived
/// trees from it on are included, which is what changed since a marking.
/// Setting imports writes a view under the current changelist, which may
/// be the last one marked, hence the inclusive bound.
///
/// Every place that keeps a tree must be listed here, or collection would
/// delete nodes still in use.
fn tree_roots(
    changes: &impl ReadableTable<u64, Bytes>,
    streams: &impl ReadableTable<&'static str, Bytes>,
    artifacts: &impl ReadableTable<u64, Bytes>,
    derived: [&impl ReadableTable<(&'static str, u64), Bytes>; 2],
    marked_through: Option<ClNumber>,
) -> Result<(Vec<TreeId>, ClNumber)> {
    let mut roots = Vec::new();
    let mut last = ClNumber(0);
    let after = marked_through.map_or(0, |m| m.0 + 1);
    for entry in changes.range(after..)? {
        let (number, value) = entry?;
        roots.push(dec::<Changelist>("changes", value.value())?.root);
        last = last.max(ClNumber(number.value()));
    }
    for entry in streams.iter()? {
        let stream: Stream = dec("streams", entry?.1.value())?;
        roots.extend([stream.root, stream.base_root]);
    }
    for entry in artifacts.iter()? {
        roots.push(dec::<Artifact>("artifacts", entry?.1.value())?.root);
    }
    for table in derived {
        for entry in table.iter()? {
            let (key, value) = entry?;
            let cl = key.value().1;
            if marked_through.is_none_or(|m| cl >= m.0) {
                roots.push(super::imports::decode_tree(value.value())?);
                last = last.max(ClNumber(cl));
            }
        }
    }
    Ok((roots, last))
}
