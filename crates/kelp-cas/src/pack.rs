//! A chunk store that packs small chunks into append-only pack files.
//!
//! Storing every small chunk as its own file costs several file system
//! operations per chunk. A [`PackStore`] instead appends small chunks to a
//! pack file and records their location in an index, so a batch of uploads
//! costs one append per chunk plus one sync and one index commit per batch.
//! Large chunks are stored as loose files.
//!
//! A pack record is the 32-byte chunk id, the compressed length as a
//! little-endian `u32`, and the zstd-compressed chunk.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use kelp_core::{ChunkId, Hash};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::store::{blocking, compress, decompress};
use crate::{BlobStore, Error, FsBlobStore, Result};

const INDEX: TableDefinition<&[u8], (u32, u64, u32)> = TableDefinition::new("chunks");
const DEAD: TableDefinition<u32, u64> = TableDefinition::new("dead_bytes");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// The version of the pack and index format this build reads and writes.
pub const PACK_FORMAT: u64 = 1;

/// Chunks at least this large are stored as loose files.
const LOOSE_THRESHOLD: usize = 256 << 10;
/// A pack stops taking new chunks once it reaches this size.
const PACK_SIZE: u64 = 256 << 20;
const RECORD_HEADER: u64 = 36;
const LEVEL: i32 = 3;

/// Where a packed chunk lives: pack number, offset of its compressed bytes,
/// and their length.
type Location = (u32, u64, u32);

/// The result of [`PackStore::compact`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
    /// Packs rewritten and removed.
    pub packs: usize,
    /// Bytes of pack files freed.
    pub freed: u64,
}

/// A [`BlobStore`] that packs small chunks; see the [module docs](self).
///
/// A packed chunk can be read as soon as it is stored, but
/// [`has`](BlobStore::has) reports it only once a [`sync`](BlobStore::sync)
/// made it durable, so that nobody skips storing a chunk that a failed sync
/// may still lose.
#[derive(Clone)]
pub struct PackStore {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    loose: FsBlobStore,
    index: Database,
    state: Mutex<State>,
    /// Packs opened for reading; opening a file per read is slow on Windows.
    readers: Mutex<HashMap<u32, Arc<File>>>,
}

#[derive(Default)]
struct State {
    /// The pack receiving new chunks, with its current length.
    active: Option<(u32, File, u64)>,
    /// Chunks appended since the last sync.
    pending: HashMap<ChunkId, Location>,
    /// Chunks whose pack failed to sync, so that they may not be on disk.
    /// Until each is stored again or deleted, [`BlobStore::sync`] fails:
    /// otherwise a caller whose chunks were dropped by another caller's
    /// failed sync would be told they are durable.
    lost: HashSet<ChunkId>,
}

impl State {
    /// Gives up on the chunks appended to `pack` but not yet indexed, after
    /// syncing it failed: the kernel may have dropped their data while a
    /// retry would report success. The pack takes no more chunks.
    fn fail_pack(&mut self, pack: u32) {
        let lost: Vec<ChunkId> = self
            .pending
            .iter()
            .filter(|(_, loc)| loc.0 == pack)
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            self.pending.remove(&id);
            self.lost.insert(id);
        }
        if self.active.as_ref().is_some_and(|(n, _, _)| *n == pack) {
            self.active = None;
        }
    }
}

impl PackStore {
    /// Opens the store in `dir`, creating it if needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the directory or index cannot be opened, or
    /// was written by a newer Kelp with a different [`PACK_FORMAT`].
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("packs"))?;
        let index = Database::create(dir.join("packs").join("index.redb")).map_err(io)?;
        let txn = index.begin_write().map_err(io)?;
        txn.open_table(INDEX).map_err(io)?;
        txn.open_table(DEAD).map_err(io)?;
        {
            let mut meta = txn.open_table(META).map_err(io)?;
            let format = meta.get("format").map_err(io)?.map(|v| v.value());
            if format.is_some_and(|f| f > PACK_FORMAT) {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} has pack format {format:?}; upgrade Kelp", dir.display()),
                )));
            }
            meta.insert("format", PACK_FORMAT).map_err(io)?;
        }
        txn.commit().map_err(io)?;
        let loose = FsBlobStore::new(dir.join("loose"));
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                loose,
                index,
                state: Mutex::default(),
                readers: Mutex::default(),
            }),
        })
    }

    /// Rewrites packs in which at least half of the bytes belong to deleted
    /// chunks, and removes them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a pack cannot be read or rewritten.
    pub async fn compact(&self) -> Result<Compaction> {
        let inner = self.inner.clone();
        blocking(move || inner.compact()).await
    }
}

impl Inner {
    fn pack_path(&self, pack: u32) -> PathBuf {
        self.dir.join("packs").join(format!("pack-{pack:08}.dat"))
    }

    fn locate(&self, id: ChunkId) -> Result<Option<Location>> {
        if let Some(loc) = self.state.lock().expect("pack lock").pending.get(&id) {
            return Ok(Some(*loc));
        }
        let txn = self.index.begin_read().map_err(io)?;
        let table = txn.open_table(INDEX).map_err(io)?;
        Ok(table.get(id.0.0.as_slice()).map_err(io)?.map(|v| v.value()))
    }

    /// Returns where each of `ids` is indexed, in one read transaction.
    ///
    /// Unlike [`locate`](Self::locate), this leaves out chunks appended
    /// since the last sync: they are not durable yet, and a caller that
    /// skips storing a chunk because it is present relies on it staying.
    fn locate_indexed(&self, ids: &[ChunkId]) -> Result<Vec<Option<Location>>> {
        let txn = self.index.begin_read().map_err(io)?;
        let table = txn.open_table(INDEX).map_err(io)?;
        ids.iter()
            .map(|id| Ok(table.get(id.0.0.as_slice()).map_err(io)?.map(|v| v.value())))
            .collect()
    }

    /// Reads the chunk `id`, from a pack or else from the loose store.
    fn get(&self, id: ChunkId) -> Result<Bytes> {
        let Some(loc) = self.locate(id)? else {
            return self.loose.read_chunk(id);
        };
        match self.read(id, loc) {
            // Compaction may have moved the chunk to another pack and
            // removed this one since it was located; look again.
            Err(Error::Io(e)) => match self.locate(id)? {
                Some(moved) if moved != loc => self.read(id, moved),
                Some(_) => Err(Error::Io(e)),
                None => self.loose.read_chunk(id),
            },
            result => result,
        }
    }

    fn reader(&self, pack: u32) -> Result<Arc<File>> {
        let mut readers = self.readers.lock().expect("readers lock");
        if let Some(file) = readers.get(&pack) {
            return Ok(file.clone());
        }
        let file = Arc::new(File::open(self.pack_path(pack))?);
        readers.insert(pack, file.clone());
        Ok(file)
    }

    fn read(&self, id: ChunkId, (pack, offset, len): Location) -> Result<Bytes> {
        let file = self.reader(pack)?;
        let mut compressed = vec![0u8; len as usize];
        read_exact_at(&file, &mut compressed, offset)?;
        let data = decompress(&compressed).map_err(|_| Error::Corrupt(id))?;
        if ChunkId::of(&data) != id {
            return Err(Error::Corrupt(id));
        }
        Ok(data.into())
    }

    fn append(&self, id: ChunkId, compressed: &[u8]) -> Result<()> {
        let mut state = self.state.lock().expect("pack lock");
        if state.pending.contains_key(&id) {
            return Ok(());
        }
        let full = state
            .active
            .as_ref()
            .is_none_or(|(_, _, len)| *len >= PACK_SIZE);
        if full {
            if let Some((pack, file, _)) = &state.active
                && let Err(e) = file.sync_data()
            {
                let pack = *pack;
                state.fail_pack(pack);
                return Err(e.into());
            }
            let next = self.next_pack()?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.pack_path(next))?;
            sync_dir(&self.dir.join("packs"))?;
            let len = file.metadata()?.len();
            state.active = Some((next, file, len));
        }
        let (pack, file, len) = state.active.as_mut().expect("active pack opened above");
        let mut record = Vec::with_capacity(RECORD_HEADER as usize + compressed.len());
        record.extend_from_slice(&id.0.0);
        record.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        record.extend_from_slice(compressed);
        if let Err(e) = file.write_all(&record) {
            // A partial record would shift every later offset; cut it off,
            // or stop using this pack if that fails too.
            if file.set_len(*len).is_err() {
                state.active = None;
            }
            return Err(e.into());
        }
        let location = (*pack, *len + RECORD_HEADER, compressed.len() as u32);
        *len += record.len() as u64;
        state.pending.insert(id, location);
        state.lost.remove(&id);
        Ok(())
    }

    /// Returns one more than the highest pack number on disk.
    fn next_pack(&self) -> Result<u32> {
        let mut highest = 0;
        for entry in fs::read_dir(self.dir.join("packs"))? {
            let name = entry?.file_name();
            if let Some(n) = name
                .to_str()
                .and_then(|n| n.strip_prefix("pack-")?.strip_suffix(".dat")?.parse().ok())
            {
                highest = highest.max(n);
            }
        }
        Ok(highest + 1)
    }

    /// Makes the chunks appended so far durable, and fails while chunks lost
    /// to an earlier failed sync have not been stored again.
    fn sync(&self) -> Result<()> {
        self.flush()?;
        let state = self.state.lock().expect("pack lock");
        match state.lost.len() {
            0 => Ok(()),
            n => Err(Error::Io(std::io::Error::other(format!(
                "{n} chunks were lost when syncing a pack failed; store them again"
            )))),
        }
    }

    /// Syncs the active pack and indexes the chunks appended so far.
    fn flush(&self) -> Result<()> {
        let mut state = self.state.lock().expect("pack lock");
        if state.pending.is_empty() {
            return Ok(());
        }
        if let Some((pack, file, _)) = &state.active
            && let Err(e) = file.sync_data()
        {
            let pack = *pack;
            state.fail_pack(pack);
            return Err(e.into());
        }
        let txn = self.index.begin_write().map_err(io)?;
        {
            let mut table = txn.open_table(INDEX).map_err(io)?;
            for (id, loc) in &state.pending {
                table.insert(id.0.0.as_slice(), *loc).map_err(io)?;
            }
        }
        txn.commit().map_err(io)?;
        state.pending.clear();
        Ok(())
    }

    fn delete(&self, id: ChunkId) -> Result<bool> {
        self.flush()?;
        self.state.lock().expect("pack lock").lost.remove(&id);
        let txn = self.index.begin_write().map_err(io)?;
        let removed = {
            let mut index = txn.open_table(INDEX).map_err(io)?;
            let removed = index
                .remove(id.0.0.as_slice())
                .map_err(io)?
                .map(|v| v.value());
            if let Some((pack, _, len)) = removed {
                let mut dead = txn.open_table(DEAD).map_err(io)?;
                let before = dead.get(pack).map_err(io)?.map_or(0, |v| v.value());
                dead.insert(pack, before + RECORD_HEADER + u64::from(len))
                    .map_err(io)?;
            }
            removed.is_some()
        };
        txn.commit().map_err(io)?;
        Ok(removed)
    }

    fn packed_ids(&self) -> Result<Vec<ChunkId>> {
        self.flush()?;
        let txn = self.index.begin_read().map_err(io)?;
        let table = txn.open_table(INDEX).map_err(io)?;
        let mut ids = Vec::with_capacity(table.len().map_err(io)? as usize);
        for entry in table.iter().map_err(io)? {
            let (key, _) = entry.map_err(io)?;
            ids.push(ChunkId(Hash(key.value().try_into().map_err(|_| {
                Error::Io(std::io::ErrorKind::InvalidData.into())
            })?)));
        }
        Ok(ids)
    }

    fn compact(&self) -> Result<Compaction> {
        self.flush()?;
        let active = self
            .state
            .lock()
            .expect("pack lock")
            .active
            .as_ref()
            .map(|(n, _, _)| *n);
        let candidates: Vec<(u32, u64)> = {
            let txn = self.index.begin_read().map_err(io)?;
            let dead = txn.open_table(DEAD).map_err(io)?;
            let mut out = Vec::new();
            for entry in dead.iter().map_err(io)? {
                let (pack, bytes) = entry.map_err(io)?;
                let (pack, bytes) = (pack.value(), bytes.value());
                let size = fs::metadata(self.pack_path(pack)).map_or(0, |m| m.len());
                if Some(pack) != active && bytes * 2 >= size {
                    out.push((pack, size));
                }
            }
            out
        };
        let mut result = Compaction::default();
        for (pack, size) in candidates {
            for (id, compressed) in self.live_records(pack)? {
                self.append(id, &compressed)?;
            }
            self.flush()?;
            let txn = self.index.begin_write().map_err(io)?;
            txn.open_table(DEAD).map_err(io)?.remove(pack).map_err(io)?;
            txn.commit().map_err(io)?;
            self.readers.lock().expect("readers lock").remove(&pack);
            fs::remove_file(self.pack_path(pack))?;
            result.packs += 1;
            result.freed += size;
        }
        Ok(result)
    }

    /// Returns the records of `pack` that the index still points at.
    ///
    /// Records are found through the index rather than by parsing the pack,
    /// so a torn record left by a crash cannot hide the ones after it.
    fn live_records(&self, pack: u32) -> Result<Vec<(ChunkId, Vec<u8>)>> {
        let file = File::open(self.pack_path(pack))?;
        let txn = self.index.begin_read().map_err(io)?;
        let index = txn.open_table(INDEX).map_err(io)?;
        let mut live = Vec::new();
        for entry in index.iter().map_err(io)? {
            let (key, loc) = entry.map_err(io)?;
            let (p, offset, len) = loc.value();
            if p != pack {
                continue;
            }
            let id =
                ChunkId(Hash(key.value().try_into().map_err(|_| {
                    Error::Io(std::io::ErrorKind::InvalidData.into())
                })?));
            let mut data = vec![0u8; len as usize];
            read_exact_at(&file, &mut data, offset)?;
            live.push((id, data));
        }
        Ok(live)
    }
}

/// Makes a new or renamed directory entry in `dir` durable.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    let _ = dir;
    Ok(())
}

impl BlobStore for PackStore {
    async fn has(&self, ids: &[ChunkId]) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(ids.len());
        let mut loose = Vec::new();
        for (i, location) in self.inner.locate_indexed(ids)?.into_iter().enumerate() {
            out.push(location.is_some());
            if location.is_none() {
                loose.push((i, ids[i]));
            }
        }
        let found = self
            .inner
            .loose
            .has(&loose.iter().map(|(_, id)| *id).collect::<Vec<_>>())
            .await?;
        for ((i, _), present) in loose.into_iter().zip(found) {
            out[i] = present;
        }
        Ok(out)
    }

    async fn get(&self, id: ChunkId) -> Result<Bytes> {
        let inner = self.inner.clone();
        blocking(move || inner.get(id)).await
    }

    async fn put(&self, id: ChunkId, data: Bytes) -> Result<()> {
        if data.len() >= LOOSE_THRESHOLD {
            return self.inner.loose.put(id, data).await;
        }
        if self.inner.locate(id)?.is_some() {
            return Ok(());
        }
        let inner = self.inner.clone();
        blocking(move || {
            let compressed = compress(&data, LEVEL)?;
            inner.append(id, &compressed)
        })
        .await
    }

    async fn delete(&self, id: ChunkId) -> Result<()> {
        let inner = self.inner.clone();
        if !blocking(move || inner.delete(id)).await? {
            self.inner.loose.delete(id).await?;
        }
        Ok(())
    }

    async fn sync(&self) -> Result<()> {
        let inner = self.inner.clone();
        blocking(move || inner.sync()).await
    }

    async fn ids(&self) -> Result<Vec<ChunkId>> {
        let inner = self.inner.clone();
        let mut ids = blocking(move || inner.packed_ids()).await?;
        ids.extend(self.inner.loose.ids().await?);
        Ok(ids)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match std::os::windows::fs::FileExt::seek_read(
            file,
            &mut buf[done..],
            offset + done as u64,
        )? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => done += n,
        }
    }
    Ok(())
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: u8) -> (ChunkId, Bytes) {
        let data = Bytes::from(vec![n; 1_000]);
        (ChunkId::of(&data), data)
    }

    #[tokio::test]
    async fn chunks_lost_to_a_failed_sync_fail_later_syncs_until_stored_again() {
        let sandbox = kelp_testkit::Sandbox::new();
        let store = PackStore::open(sandbox.dir("store")).unwrap();
        let (a, b) = (chunk(1), chunk(2));
        store.put(a.0, a.1.clone()).await.unwrap();
        assert_eq!(
            store.has(&[a.0]).await.unwrap(),
            [false],
            "not reported before it is durable"
        );
        store.put(b.0, b.1.clone()).await.unwrap();

        // What a failed fsync of the active pack does.
        {
            let mut state = store.inner.state.lock().unwrap();
            let pack = state.active.as_ref().unwrap().0;
            state.fail_pack(pack);
        }
        assert_eq!(store.has(&[a.0, b.0]).await.unwrap(), [false, false]);
        assert!(store.sync().await.is_err());
        assert!(store.sync().await.is_err(), "the failure sticks");

        store.put(a.0, a.1.clone()).await.unwrap();
        assert!(store.sync().await.is_err(), "b is still lost");
        store.delete(b.0).await.unwrap();
        store.sync().await.unwrap();
        assert_eq!(store.has(&[a.0, b.0]).await.unwrap(), [true, false]);
        assert_eq!(store.get(a.0).await.unwrap(), a.1);
    }
}
