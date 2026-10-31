//! Chunk storage backends.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use kelp_core::ChunkId;

use crate::{Error, Result};

/// A store of chunks addressed by [`ChunkId`].
///
/// The local file system is the only backend today. S3, tiered hot/cold
/// storage, and caching wrappers for edges and clients are planned.
pub trait BlobStore: Send + Sync {
    /// Returns, for each id in `ids`, whether the store already holds it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the backend cannot be queried.
    fn has(&self, ids: &[ChunkId]) -> impl Future<Output = Result<Vec<bool>>> + Send;

    /// Returns the uncompressed content of the chunk `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the chunk is absent, [`Error::Corrupt`] if
    /// its stored bytes do not decode to content hashing to `id`, and
    /// [`Error::Io`] on other backend failures.
    fn get(&self, id: ChunkId) -> impl Future<Output = Result<Bytes>> + Send;

    /// Stores `data` under `id`.
    ///
    /// Storing a chunk that already exists is a no-op. The caller is responsible
    /// for `id` being the hash of `data`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the chunk cannot be written.
    fn put(&self, id: ChunkId, data: Bytes) -> impl Future<Output = Result<()>> + Send;

    /// Removes the chunk `id`.
    ///
    /// Deleting a chunk that does not exist is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the chunk cannot be removed.
    fn delete(&self, id: ChunkId) -> impl Future<Output = Result<()>> + Send;

    /// Makes every chunk stored so far durable.
    ///
    /// Stores that make each [`put`](Self::put) durable on its own need not
    /// override this.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the chunks cannot be synced.
    fn sync(&self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Returns the ids of all stored chunks, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the store cannot be listed.
    fn ids(&self) -> impl Future<Output = Result<Vec<ChunkId>>> + Send;
}

/// A [`BlobStore`] that keeps zstd-compressed chunks under `root/ab/cd/<hex>`.
///
/// Writes go to a temporary file that is renamed into place, so readers never
/// observe a partially written chunk. A durable store also syncs each chunk
/// to disk before the rename.
#[derive(Clone, Debug)]
pub struct FsBlobStore {
    root: PathBuf,
    level: i32,
    durable: bool,
}

impl FsBlobStore {
    /// Creates a store rooted at `root`.
    ///
    /// The directory is created lazily on the first [`put`](BlobStore::put).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            level: 3,
            durable: true,
        }
    }

    /// Creates a store that does not sync chunks to disk, for caches whose
    /// content can be fetched again after a crash.
    pub fn cache(root: impl Into<PathBuf>) -> Self {
        Self {
            durable: false,
            ..Self::new(root)
        }
    }

    /// Returns the uncompressed size of a stored chunk without decompressing
    /// it, or `None` if the chunk is absent or its size is not recorded.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the chunk exists but cannot be read.
    pub fn stored_size(&self, id: ChunkId) -> Result<Option<u64>> {
        use std::io::Read;
        let mut header = [0u8; 18];
        let n = match std::fs::File::open(self.object_path(id)) {
            Ok(mut f) => f.read(&mut header)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(zstd::zstd_safe::get_frame_content_size(&header[..n])
            .ok()
            .flatten())
    }

    /// Makes the stored chunk `id` durable, even in a store created with
    /// [`cache`](Self::cache).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the chunk is absent, or [`Error::Io`]
    /// if it cannot be synced.
    pub fn persist(&self, id: ChunkId) -> Result<()> {
        let path = self.object_path(id);
        // Flushing needs write access on Windows.
        match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(f) => f.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound(id)),
            Err(e) => return Err(e.into()),
        }
        crate::pack::sync_dir(path.parent().expect("object path has parent"))
    }

    fn object_path(&self, id: ChunkId) -> PathBuf {
        let hex = id.to_string();
        self.root.join(&hex[0..2]).join(&hex[2..4]).join(hex)
    }
}

impl BlobStore for FsBlobStore {
    async fn has(&self, ids: &[ChunkId]) -> Result<Vec<bool>> {
        let paths: Vec<PathBuf> = ids.iter().map(|id| self.object_path(*id)).collect();
        blocking(move || {
            paths
                .iter()
                .map(|p| p.try_exists().map_err(Error::from))
                .collect()
        })
        .await
    }

    async fn get(&self, id: ChunkId) -> Result<Bytes> {
        let path = self.object_path(id);
        blocking(move || {
            let compressed = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::NotFound(id));
                }
                Err(e) => return Err(e.into()),
            };
            let data = decompress(&compressed).map_err(|_| Error::Corrupt(id))?;
            if ChunkId::of(&data) != id {
                return Err(Error::Corrupt(id));
            }
            Ok(data.into())
        })
        .await
    }

    async fn put(&self, id: ChunkId, data: Bytes) -> Result<()> {
        let (path, level, durable) = (self.object_path(id), self.level, self.durable);
        blocking(move || {
            // An empty file is what a crash before the data reached the
            // disk leaves behind; replace it rather than trust it.
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
                return Ok(());
            }
            let dir = path.parent().expect("object path has parent");
            let created = !dir.exists();
            let compressed = compress(&data, level)?;
            let tmp = temp_name(dir, id);
            let mut file = match std::fs::File::create(&tmp) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir_all(dir)?;
                    std::fs::File::create(&tmp)?
                }
                other => other?,
            };
            file.write_all(&compressed)?;
            if durable {
                file.sync_all()?;
            }
            drop(file);
            if let Err(e) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                // A concurrent put of the same chunk got there first; on
                // Windows, replacing its file can fail while it is open.
                if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
                    return Ok(());
                }
                return Err(e.into());
            }
            if durable {
                crate::pack::sync_dir(dir)?;
                if created {
                    for ancestor in dir.ancestors().skip(1).take(2) {
                        crate::pack::sync_dir(ancestor)?;
                    }
                }
            }
            Ok(())
        })
        .await
    }

    async fn ids(&self) -> Result<Vec<ChunkId>> {
        let root = self.root.clone();
        blocking(move || {
            let mut ids = Vec::new();
            let mut dirs = vec![root];
            while let Some(dir) = dirs.pop() {
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        dirs.push(entry.path());
                    } else if let Some(id) = entry.file_name().to_str().and_then(parse_id) {
                        ids.push(id);
                    }
                }
            }
            Ok(ids)
        })
        .await
    }

    async fn delete(&self, id: ChunkId) -> Result<()> {
        let path = self.object_path(id);
        blocking(move || match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        })
        .await
    }
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))?
}

thread_local! {
    static COMPRESSOR: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> = const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

/// Compresses with a per-thread context; creating a zstd context costs more
/// than compressing a typical small chunk.
pub(crate) fn compress(data: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    COMPRESSOR.with_borrow_mut(|slot| {
        if slot.as_ref().is_none_or(|(l, _)| *l != level) {
            *slot = Some((level, zstd::bulk::Compressor::new(level)?));
        }
        let (_, compressor) = slot.as_mut().expect("compressor initialised");
        compressor.include_contentsize(true)?;
        compressor.compress(data)
    })
}

pub(crate) fn decompress(compressed: &[u8]) -> std::io::Result<Vec<u8>> {
    let Some(size) = zstd::zstd_safe::get_frame_content_size(compressed)
        .ok()
        .flatten()
    else {
        return zstd::decode_all(compressed);
    };
    DECOMPRESSOR.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new()?);
        }
        slot.as_mut()
            .expect("decompressor initialised")
            .decompress(compressed, size as usize)
    })
}

/// Returns a temporary path for `id` that no other put uses, even one of the
/// same chunk at the same time: the clock alone does not tell them apart.
fn temp_name(dir: &Path, id: ChunkId) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".tmp-{id}-{}-{n}", std::process::id()))
}

/// Parses a chunk id from its 64-digit hex form.
pub(crate) fn parse_id(hex: &str) -> Option<ChunkId> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(ChunkId(kelp_core::Hash(bytes)))
}
