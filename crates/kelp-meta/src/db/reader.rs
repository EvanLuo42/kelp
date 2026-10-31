use kelp_core::{ClNumber, ContentId, DepotPath, FileManifest, Hash, TreeId};
use redb::{ReadTransaction, ReadableTable};

use super::nodes::ReadNodes;
use super::{
    CHANGES, DEPOTS, GROUPS, INTEGRATIONS, LAST_INDEX, LOCKS, MANIFESTS, META, NODES, PROTECTIONS,
    REVISIONS, SETTINGS, SHELVES, STREAM_CHANGES, STREAMS, TOKENS, TYPEMAPS, USERS, dec, name_key,
};
use crate::acl::Protections;
use crate::record::{
    Changelist, Depot, Group, Revision, Shelf, Stream, Token, TypemapRecord, User,
};
use crate::tree::fold;
use crate::{Error, Lock, LockDomain, LogIndex, Result};

/// A consistent read snapshot.
pub struct Reader {
    txn: ReadTransaction,
}

impl Reader {
    pub(super) fn new(txn: ReadTransaction) -> Self {
        Self { txn }
    }

    pub(super) fn txn(&self) -> &ReadTransaction {
        &self.txn
    }

    /// Returns the index of the last applied command, or `LogIndex(0)` if none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn last_index(&self) -> Result<LogIndex> {
        Ok(LogIndex(
            self.txn
                .open_table(META)?
                .get(LAST_INDEX)?
                .map_or(0, |v| v.value()),
        ))
    }

    /// Returns a digest of every stream's head changelist and tree, which
    /// equal databases agree on.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn stream_digest(&self) -> Result<kelp_core::Hash> {
        let mut h = blake3::Hasher::new();
        for stream in self.streams()? {
            h.update(stream.path.key().as_bytes());
            h.update(&[0]);
            h.update(&stream.head.0.to_le_bytes());
            h.update(&stream.root.0.0);
        }
        Ok(kelp_core::Hash(*h.finalize().as_bytes()))
    }

    /// Returns the user named `name`, compared case-insensitively.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read, or
    /// [`Error::CorruptRecord`] if the record cannot be decoded.
    pub fn user(&self, name: &str) -> Result<Option<User>> {
        let users = self.txn.open_table(USERS)?;
        users
            .get(name_key(name).as_str())?
            .map(|v| dec("users", v.value()))
            .transpose()
    }

    /// Returns the database's format version; see [`FORMAT_VERSION`].
    ///
    /// [`FORMAT_VERSION`]: super::FORMAT_VERSION
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn format_version(&self) -> Result<u64> {
        Ok(self
            .txn
            .open_table(META)?
            .get(super::FORMAT)?
            .map_or(super::FORMAT_VERSION, |v| v.value()))
    }

    /// Returns `true` if at least one user exists.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn has_users(&self) -> Result<bool> {
        Ok(self.txn.open_table(USERS)?.iter()?.next().is_some())
    }

    /// Returns the token stored under `token_hash`. Expiry is not checked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read, or
    /// [`Error::CorruptRecord`] if the record cannot be decoded.
    pub fn token(&self, token_hash: &Hash) -> Result<Option<Token>> {
        let tokens = self.txn.open_table(TOKENS)?;
        tokens
            .get(token_hash.0.as_slice())?
            .map(|v| dec("tokens", v.value()))
            .transpose()
    }

    /// Returns all groups, ordered by folded name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn groups(&self) -> Result<Vec<Group>> {
        let groups = self.txn.open_table(GROUPS)?;
        groups
            .iter()?
            .map(|r| dec("groups", r?.1.value()))
            .collect()
    }

    /// Returns the names of the groups `user` belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn groups_of(&self, user: &str) -> Result<Vec<String>> {
        let user = name_key(user);
        Ok(self
            .groups()?
            .into_iter()
            .filter(|g| g.members.iter().any(|m| name_key(m) == user))
            .map(|g| g.name)
            .collect())
    }

    /// Returns the protections table, or [`Protections::open`] if none was set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read, or
    /// [`Error::CorruptRecord`] if the stored table is invalid.
    pub fn protections(&self) -> Result<Protections> {
        match self.protections_source()? {
            Some(source) => {
                Protections::parse(&source).map_err(|_| Error::CorruptRecord("settings"))
            }
            None => Ok(Protections::open()),
        }
    }

    /// Returns the source of the protections table, if one was set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn protections_source(&self) -> Result<Option<String>> {
        let settings = self.txn.open_table(SETTINGS)?;
        Ok(settings
            .get(PROTECTIONS)?
            .map(|v| String::from_utf8_lossy(v.value()).into_owned()))
    }

    /// Returns all depots, ordered by folded name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn depots(&self) -> Result<Vec<Depot>> {
        let depots = self.txn.open_table(DEPOTS)?;
        depots
            .iter()?
            .map(|r| dec("depots", r?.1.value()))
            .collect()
    }

    /// Returns the typemap of `depot`, if one was set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn typemap(&self, depot: &str) -> Result<Option<TypemapRecord>> {
        let typemaps = self.txn.open_table(TYPEMAPS)?;
        typemaps
            .get(name_key(depot).as_str())?
            .map(|v| dec("typemaps", v.value()))
            .transpose()
    }

    /// Returns the stream at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn stream(&self, path: &DepotPath) -> Result<Option<Stream>> {
        let streams = self.txn.open_table(STREAMS)?;
        streams
            .get(path.key())?
            .map(|v| dec("streams", v.value()))
            .transpose()
    }

    /// Returns all streams, ordered by folded path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn streams(&self) -> Result<Vec<Stream>> {
        let streams = self.txn.open_table(STREAMS)?;
        streams
            .iter()?
            .map(|r| dec("streams", r?.1.value()))
            .collect()
    }

    /// Returns the last changelist of `source` that `target` has integrated.
    ///
    /// A stream created from a parent has integrated the parent's head at the
    /// time of creation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn integration(&self, target: &DepotPath, source: &DepotPath) -> Result<Option<ClNumber>> {
        let table = self.txn.open_table(INTEGRATIONS)?;
        Ok(table
            .get((target.key(), source.key()))?
            .map(|v| ClNumber(v.value())))
    }

    /// Returns the submitted changelist numbered `cl`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn changelist(&self, cl: ClNumber) -> Result<Option<Changelist>> {
        let changes = self.txn.open_table(CHANGES)?;
        changes
            .get(cl.0)?
            .map(|v| dec("changes", v.value()))
            .transpose()
    }

    /// Returns the latest changelist of `stream` at or before `at`, and the
    /// stream's file tree as of that changelist.
    ///
    /// A `None` or future `at` means the stream head. If no changelist was
    /// submitted to the stream by `at`, returns `ClNumber(0)` and the tree the
    /// stream was created with.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream does not exist, and
    /// [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn stream_at(
        &self,
        stream: &DepotPath,
        at: Option<ClNumber>,
    ) -> Result<(ClNumber, TreeId)> {
        let s = self
            .stream(stream)?
            .ok_or_else(|| Error::NotFound(format!("stream {stream}")))?;
        let Some(at) = at.filter(|at| *at < s.head) else {
            return Ok((s.head, s.root));
        };
        let index = self.txn.open_table(STREAM_CHANGES)?;
        let last = index
            .range((s.path.key(), 0u64)..=(s.path.key(), at.0))?
            .next_back()
            .transpose()?;
        match last {
            Some((k, _)) => {
                let cl = ClNumber(k.value().1);
                // A changelist of another stream changed this one through a
                // writable import.
                if let Some(v) = self
                    .txn
                    .open_table(super::STREAM_ROOTS)?
                    .get((s.path.key(), cl.0))?
                {
                    return Ok((cl, super::imports::decode_tree(v.value())?));
                }
                let record = self
                    .changelist(cl)?
                    .ok_or_else(|| Error::NotFound(format!("changelist {cl}")))?;
                Ok((cl, record.root))
            }
            None => Ok((ClNumber(0), s.base_root)),
        }
    }

    /// Returns up to `limit` changelists, most recent first, optionally limited
    /// to one stream.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn changes(&self, stream: Option<&DepotPath>, limit: usize) -> Result<Vec<Changelist>> {
        let numbers: Vec<u64> = match stream {
            Some(s) => {
                let index = self.txn.open_table(STREAM_CHANGES)?;
                let range = index.range((s.key(), 0u64)..=(s.key(), u64::MAX))?;
                range
                    .rev()
                    .take(limit)
                    .map(|r| Ok(r?.0.value().1))
                    .collect::<Result<_>>()?
            }
            None => {
                let changes = self.txn.open_table(CHANGES)?;
                changes
                    .iter()?
                    .rev()
                    .take(limit)
                    .map(|r| Ok(r?.0.value()))
                    .collect::<Result<_>>()?
            }
        };
        numbers
            .into_iter()
            .map(|n| {
                self.changelist(ClNumber(n))?
                    .ok_or_else(|| Error::NotFound(format!("changelist {n}")))
            })
            .collect()
    }

    /// Returns the revisions of the file at `path`, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn revisions(&self, path: &DepotPath) -> Result<Vec<Revision>> {
        let revisions = self.txn.open_table(REVISIONS)?;
        let range = revisions.range((path.key(), 0u32)..=(path.key(), u32::MAX))?;
        range
            .rev()
            .map(|r| dec("revisions", r?.1.value()))
            .collect()
    }

    /// Returns the shelf numbered `cl`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn shelf(&self, cl: ClNumber) -> Result<Option<Shelf>> {
        let shelves = self.txn.open_table(SHELVES)?;
        shelves
            .get(cl.0)?
            .map(|v| dec("shelves", v.value()))
            .transpose()
    }

    /// Returns all locks in `domain`, ordered by folded path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn locks(&self, domain: &LockDomain) -> Result<Vec<Lock>> {
        let locks = self.txn.open_table(LOCKS)?;
        let range = locks.range((domain.0.as_str(), "")..)?;
        let mut out = Vec::new();
        for entry in range {
            let (key, value) = entry?;
            if key.value().0 != domain.0 {
                break;
            }
            out.push(dec("locks", value.value())?);
        }
        Ok(out)
    }

    /// Returns the lock on the stream-relative `path` in `domain`, if any.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn lock(&self, domain: &LockDomain, path: &str) -> Result<Option<Lock>> {
        let locks = self.txn.open_table(LOCKS)?;
        locks
            .get((domain.0.as_str(), fold(path).as_str()))?
            .map(|v| dec("locks", v.value()))
            .transpose()
    }

    /// Returns the manifest stored for `content`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn manifest(&self, content: &ContentId) -> Result<Option<FileManifest>> {
        let manifests = self.txn.open_table(MANIFESTS)?;
        manifests
            .get(content.0.0.as_slice())?
            .map(|v| dec("manifests", v.value()))
            .transpose()
    }

    /// Returns a node store over this snapshot for reading file trees.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be opened.
    pub fn nodes(&self) -> Result<ReadNodes> {
        Ok(ReadNodes::new(self.txn.open_table(NODES)?))
    }
}
