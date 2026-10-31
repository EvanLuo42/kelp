//! Raft storage: the log in its own database, and the metadata database as
//! the state machine.

// openraft's storage traits fix the error type.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kelp_meta::MetaDb;
use kelp_meta::db::FORMAT_VERSION;
use openraft::storage::{LogFlushed, RaftLogStorage, RaftStateMachine};
use openraft::{
    AnyError, Entry, EntryPayload, LogId, LogState, OptionalSend, RaftLogReader,
    RaftSnapshotBuilder, Snapshot, SnapshotMeta, StorageError, StorageIOError, StoredMembership,
    Vote,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{NodeId, Rejection, Response, TypeConfig};

const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");
const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");

const VOTE: &str = "vote";
const PURGED: &str = "last_purged";
const COMMITTED: &str = "committed";
/// The database format the log's entries are encoded with.
const FORMAT: &str = "format";

/// The consensus entries of the metadata database.
const APPLIED: &str = "raft_applied";
const MEMBERSHIP: &str = "raft_membership";

type StorageResult<T> = Result<T, StorageError<NodeId>>;

fn failed(e: impl std::fmt::Display) -> StorageError<NodeId> {
    StorageIOError::write(AnyError::error(e)).into()
}

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("raft records serialize")
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> StorageResult<T> {
    postcard::from_bytes(bytes).map_err(failed)
}

/// The Raft log, kept in `raft.redb` next to the metadata database.
#[derive(Clone)]
pub(crate) struct LogStore {
    db: Arc<Database>,
}

impl LogStore {
    pub(crate) fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        txn.open_table(ENTRIES)?;
        txn.open_table(STATE)?;
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Returns whether the log has never held anything.
    pub(crate) fn is_empty(&self) -> StorageResult<bool> {
        Ok(self.get::<Vote<NodeId>>(VOTE)?.is_none() && self.last_entry()?.is_none())
    }

    fn get<T: DeserializeOwned>(&self, key: &str) -> StorageResult<Option<T>> {
        let txn = self.db.begin_read().map_err(failed)?;
        let table = txn.open_table(STATE).map_err(failed)?;
        let value = table.get(key).map_err(failed)?;
        value.map(|v| decode(v.value())).transpose()
    }

    fn put<T: Serialize>(&self, key: &str, value: &T) -> StorageResult<()> {
        let txn = self.db.begin_write().map_err(failed)?;
        txn.open_table(STATE)
            .map_err(failed)?
            .insert(key, encode(value).as_slice())
            .map_err(failed)?;
        txn.commit().map_err(failed)
    }

    fn last_entry(&self) -> StorageResult<Option<Entry<TypeConfig>>> {
        let txn = self.db.begin_read().map_err(failed)?;
        let table = txn.open_table(ENTRIES).map_err(failed)?;
        let last = table.last().map_err(failed)?;
        last.map(|(_, v)| decode(v.value())).transpose()
    }

    /// Returns the index of the last entry without decoding it.
    fn last_index(&self) -> StorageResult<Option<u64>> {
        let txn = self.db.begin_read().map_err(failed)?;
        let table = txn.open_table(ENTRIES).map_err(failed)?;
        Ok(table.last().map_err(failed)?.map(|(k, _)| k.value()))
    }

    /// Brings a log written by a build with another database format to this
    /// build's, before Raft reads it.
    ///
    /// Entries hold commands as that format encoded them, which the
    /// migration of the metadata database does not reach. So an older log
    /// is accepted only once this member has applied all of it: its entries
    /// are then dropped, and a snapshot of the migrated database stands in
    /// for them.
    pub(crate) fn upgrade(&self, machine: &StateMachine) -> crate::Result<()> {
        let error = |e: String| crate::Error::Config(format!("raft log: {e}"));
        let storage = |e: StorageError<NodeId>| error(e.to_string());
        let format = self.get::<u64>(FORMAT).map_err(storage)?;
        match format {
            Some(f) if f == FORMAT_VERSION => return Ok(()),
            Some(f) if f > FORMAT_VERSION => {
                return Err(error(format!(
                    "it has format {f}, but this build only understands up to {FORMAT_VERSION}; \
                     upgrade Kelp"
                )));
            }
            _ => {}
        }
        let last = self.last_index().map_err(storage)?;
        let applied = machine.applied().map_err(storage)?;
        if let Some(last) = last
            && applied.is_none_or(|a| a.index < last)
        {
            let applied = applied.map_or(0, |a| a.index);
            return Err(error(format!(
                "it holds entries up to {last} in an older format, but this member has applied \
                 only up to {applied}; run the previous Kelp version on every member until each \
                 has applied the whole log, then upgrade"
            )));
        }
        match applied {
            Some(_) => {
                machine.write_snapshot().map_err(storage)?;
            }
            None => {
                for old in [machine.current(), machine.incoming()] {
                    match std::fs::remove_file(&old) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                        _ => {}
                    }
                }
            }
        }
        self.restart_at(applied).map_err(storage)
    }

    /// Drops every entry, as purged up to `applied`, and stamps the log
    /// with this build's format.
    fn restart_at(&self, applied: Option<LogId<NodeId>>) -> StorageResult<()> {
        let txn = self.db.begin_write().map_err(failed)?;
        {
            let mut state = txn.open_table(STATE).map_err(failed)?;
            if let Some(applied) = applied {
                state
                    .insert(PURGED, encode(&applied).as_slice())
                    .map_err(failed)?;
            }
            state
                .insert(FORMAT, encode(&FORMAT_VERSION).as_slice())
                .map_err(failed)?;
            txn.open_table(ENTRIES)
                .map_err(failed)?
                .retain(|_, _| false)
                .map_err(failed)?;
        }
        txn.commit().map_err(failed)
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> StorageResult<Vec<Entry<TypeConfig>>> {
        let txn = self.db.begin_read().map_err(failed)?;
        let table = txn.open_table(ENTRIES).map_err(failed)?;
        let mut entries = Vec::new();
        for row in table.range(range).map_err(failed)? {
            entries.push(decode(row.map_err(failed)?.1.value())?);
        }
        Ok(entries)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> StorageResult<LogState<TypeConfig>> {
        let last_purged_log_id = self.get::<LogId<NodeId>>(PURGED)?;
        let last_log_id = match self.last_entry()? {
            Some(entry) => Some(entry.log_id),
            None => last_purged_log_id,
        };
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> StorageResult<()> {
        self.put(VOTE, vote)
    }

    async fn read_vote(&mut self) -> StorageResult<Option<Vote<NodeId>>> {
        self.get(VOTE)
    }

    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> StorageResult<()> {
        self.put(COMMITTED, &committed)
    }

    async fn read_committed(&mut self) -> StorageResult<Option<LogId<NodeId>>> {
        Ok(self.get::<Option<LogId<NodeId>>>(COMMITTED)?.flatten())
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> StorageResult<()>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let txn = self.db.begin_write().map_err(failed)?;
        {
            let mut table = txn.open_table(ENTRIES).map_err(failed)?;
            for entry in entries {
                table
                    .insert(entry.log_id.index, encode(&entry).as_slice())
                    .map_err(failed)?;
            }
        }
        txn.commit().map_err(failed)?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> StorageResult<()> {
        let txn = self.db.begin_write().map_err(failed)?;
        txn.open_table(ENTRIES)
            .map_err(failed)?
            .retain(|index, _| index < log_id.index)
            .map_err(failed)?;
        txn.commit().map_err(failed)
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> StorageResult<()> {
        let txn = self.db.begin_write().map_err(failed)?;
        txn.open_table(STATE)
            .map_err(failed)?
            .insert(PURGED, encode(&log_id).as_slice())
            .map_err(failed)?;
        txn.open_table(ENTRIES)
            .map_err(failed)?
            .retain(|index, _| index > log_id.index)
            .map_err(failed)?;
        txn.commit().map_err(failed)
    }
}

/// The metadata database as a Raft state machine.
///
/// Each entry is applied together with the record of how far the log has
/// been applied, in one transaction, so the database never applies an entry
/// twice. The latest snapshot is kept in `raft-snapshot/` until the next.
#[derive(Clone)]
pub(crate) struct StateMachine {
    db: Arc<MetaDb>,
    dir: PathBuf,
    /// Held while the snapshot files are read or replaced.
    snapshots: Arc<tokio::sync::Mutex<()>>,
}

impl StateMachine {
    pub(crate) fn new(db: Arc<MetaDb>, dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            db,
            dir,
            snapshots: Arc::default(),
        })
    }

    /// Returns the last applied log id recorded in the database.
    pub(crate) fn applied(&self) -> StorageResult<Option<LogId<NodeId>>> {
        let reader = self.db.read().map_err(failed)?;
        match reader.consensus(APPLIED).map_err(failed)? {
            Some(bytes) => decode(&bytes),
            None => Ok(None),
        }
    }

    fn membership(&self) -> StorageResult<StoredMembership<NodeId, openraft::BasicNode>> {
        let reader = self.db.read().map_err(failed)?;
        match reader.consensus(MEMBERSHIP).map_err(failed)? {
            Some(bytes) => decode(&bytes),
            None => Ok(StoredMembership::default()),
        }
    }

    fn current(&self) -> PathBuf {
        self.dir.join("current.redb")
    }

    /// Writes a snapshot of the database as the current one; the caller
    /// keeps others from reading or replacing it meanwhile.
    fn write_snapshot(&self) -> StorageResult<SnapshotMeta<NodeId, openraft::BasicNode>> {
        let building = self.dir.join("building.redb");
        let _ = std::fs::remove_file(&building);
        self.db.snapshot_to(&building).map_err(failed)?;
        let meta = describe(&building)?;
        std::fs::rename(&building, self.current()).map_err(failed)?;
        Ok(meta)
    }

    /// Where a snapshot from the leader is received; openraft receives one
    /// at a time.
    fn incoming(&self) -> PathBuf {
        self.dir.join("incoming.redb")
    }
}

/// Describes the snapshot database at `path` from what it records itself,
/// so a snapshot and its description can never disagree.
fn describe(path: &Path) -> StorageResult<SnapshotMeta<NodeId, openraft::BasicNode>> {
    const CONSENSUS: TableDefinition<&str, &[u8]> = TableDefinition::new("consensus");
    // Only read: the file may be being sent to another member meanwhile.
    let db = Database::open(path).map_err(failed)?;
    let txn = db.begin_read().map_err(failed)?;
    let table = txn.open_table(CONSENSUS).map_err(failed)?;
    let read = |key: &str| -> StorageResult<Option<Vec<u8>>> {
        Ok(table.get(key).map_err(failed)?.map(|v| v.value().to_vec()))
    };
    let last_log_id: Option<LogId<NodeId>> = match read(APPLIED)? {
        Some(bytes) => decode(&bytes)?,
        None => None,
    };
    let last_membership = match read(MEMBERSHIP)? {
        Some(bytes) => decode(&bytes)?,
        None => StoredMembership::default(),
    };
    let snapshot_id = match last_log_id {
        Some(l) => format!("{}-{}-{}", l.leader_id.term, l.leader_id.node_id, l.index),
        None => "empty".into(),
    };
    Ok(SnapshotMeta {
        last_log_id,
        last_membership,
        snapshot_id,
    })
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachine {
    async fn build_snapshot(&mut self) -> StorageResult<Snapshot<TypeConfig>> {
        let machine = self.clone();
        let guard = self.snapshots.clone().lock_owned().await;
        let meta = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            machine.write_snapshot()
        })
        .await
        .map_err(failed)??;
        let file = tokio::fs::File::open(self.current())
            .await
            .map_err(failed)?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(file),
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> StorageResult<(
        Option<LogId<NodeId>>,
        StoredMembership<NodeId, openraft::BasicNode>,
    )> {
        Ok((self.applied()?, self.membership()?))
    }

    async fn apply<I>(&mut self, entries: I) -> StorageResult<Vec<Response>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut responses = Vec::with_capacity(entries.len());
            for entry in entries {
                let mut consensus = BTreeMap::new();
                consensus.insert(APPLIED.to_owned(), encode(&Some(entry.log_id)));
                let proposal = match entry.payload {
                    EntryPayload::Blank => None,
                    EntryPayload::Normal(proposal) => Some(proposal),
                    EntryPayload::Membership(membership) => {
                        let stored = StoredMembership::new(Some(entry.log_id), membership);
                        consensus.insert(MEMBERSHIP.to_owned(), encode(&stored));
                        None
                    }
                };
                let result = match db.apply_with_consensus(proposal.as_ref(), &consensus) {
                    Ok(outcome) => Ok(outcome),
                    // The database itself failed, not the proposal; carrying
                    // on would make this member differ from the others.
                    Err(kelp_meta::Error::Db(e)) => return Err(failed(e)),
                    Err(e) => Err(Rejection::from(crate::handler::meta_error(e))),
                };
                responses.push(Response(result));
            }
            Ok(responses)
        })
        .await
        .map_err(failed)?
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> StorageResult<Box<tokio::fs::File>> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(self.incoming())
            .await
            .map_err(failed)?;
        Ok(Box::new(file))
    }

    async fn install_snapshot(
        &mut self,
        _meta: &SnapshotMeta<NodeId, openraft::BasicNode>,
        snapshot: Box<tokio::fs::File>,
    ) -> StorageResult<()> {
        snapshot.sync_all().await.map_err(failed)?;
        drop(snapshot);
        let machine = self.clone();
        let guard = self.snapshots.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let incoming = machine.incoming();
            machine.db.replace_with(&incoming).map_err(failed)?;
            std::fs::rename(&incoming, machine.current()).map_err(failed)
        })
        .await
        .map_err(failed)?
    }

    async fn get_current_snapshot(&mut self) -> StorageResult<Option<Snapshot<TypeConfig>>> {
        let machine = self.clone();
        let guard = self.snapshots.clone().lock_owned().await;
        let meta = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let current = machine.current();
            if !current.exists() {
                return Ok(None);
            }
            describe(&current).map(Some)
        })
        .await
        .map_err(failed)??;
        let Some(meta) = meta else {
            return Ok(None);
        };
        let file = tokio::fs::File::open(self.current())
            .await
            .map_err(failed)?;
        Ok(Some(Snapshot {
            meta,
            snapshot: Box::new(file),
        }))
    }
}
