use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use kelp_cas::{BlobStore, Chunker, FastCdcChunker, FsBlobStore, build_manifest};
use kelp_core::{ChunkId, ChunkRef, ContentId, FileManifest};
use kelp_proto::{Reply, Request};

use crate::progress::{Meter, Phase, Progress, Quiet};
use crate::{Connection, Error, Result};

const QUERY_BATCH: usize = 4096;
const MANIFEST_BATCH: usize = 1024;
const PARALLEL_PUTS: usize = 16;
/// Chunks moved to or from the server at a time, so that progress shows and
/// memory stays bounded.
const TRANSFER_CHUNKS: usize = 4096;
const TRANSFER_BYTES: u64 = 64 << 20;

fn join_error(e: tokio::task::JoinError) -> Error {
    Error::Invalid(format!("cache task failed: {e}"))
}

/// File content stored on the local machine.
///
/// The cache holds chunks and manifests so that workspaces can materialise
/// files, revert edits and skip downloads for content they already have.
#[derive(Clone)]
pub struct ContentCache {
    chunks: FsBlobStore,
    manifests: PathBuf,
    progress: Arc<dyn Progress>,
}

impl ContentCache {
    /// Opens the cache rooted at `dir`, creating it lazily.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            chunks: FsBlobStore::cache(dir.join("chunks")),
            manifests: dir.join("manifests"),
            progress: Arc::new(Quiet),
        }
    }

    /// Reports the progress of transfers, and of workspace operations using
    /// this cache, to `progress`.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn Progress>) -> Self {
        self.progress = progress;
        self
    }

    /// Starts reporting the progress of `phase`.
    pub(crate) fn meter(&self, phase: Phase, items: u64, bytes: Option<u64>) -> Meter {
        Meter::start(self.progress.clone(), phase, items, bytes)
    }

    /// Returns the cached manifest of `content`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest exists but cannot be read.
    pub fn manifest(&self, content: &ContentId) -> Result<Option<FileManifest>> {
        // A file stored as one chunk has no manifest file: its chunk id is its content id.
        let single = ChunkId(content.0);
        if let Some(size) = self.chunks.stored_size(single)? {
            let chunks = vec![ChunkRef {
                id: single,
                len: size as u32,
            }];
            return Ok(Some(FileManifest {
                content: *content,
                size,
                chunks,
            }));
        }
        match fs::read(self.manifest_path(content)) {
            Ok(bytes) => postcard::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| Error::Invalid(format!("corrupt cached manifest {content}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Stores a manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest cannot be written.
    pub fn put_manifest(&self, manifest: &FileManifest) -> Result<()> {
        self.write_manifest(manifest, false)
    }

    fn write_manifest(&self, manifest: &FileManifest, durable: bool) -> Result<()> {
        if is_single_chunk(manifest) {
            return Ok(());
        }
        let path = self.manifest_path(&manifest.content);
        let dir = path.parent().expect("manifest path has a parent");
        fs::create_dir_all(dir)?;
        let tmp = unique_temp(dir);
        let mut file = fs::File::create(&tmp)?;
        std::io::Write::write_all(
            &mut file,
            &postcard::to_allocvec(manifest).expect("manifest serializes"),
        )?;
        if durable {
            file.sync_all()?;
        }
        drop(file);
        fs::rename(tmp, &path)?;
        if durable {
            #[cfg(unix)]
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Like [`ingest`](Self::ingest), and makes the content durable: it may be
    /// the only copy of someone's work, as in a snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache cannot be written or synced.
    pub async fn ingest_durable(&self, data: &[u8]) -> Result<FileManifest> {
        let manifest = self.ingest(data).await?;
        for chunk in &manifest.chunks {
            self.chunks.persist(chunk.id)?;
        }
        self.write_manifest(&manifest, true)?;
        Ok(manifest)
    }

    /// Chunks `data`, stores its chunks and manifest, and returns the manifest.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache cannot be written.
    pub async fn ingest(&self, data: &[u8]) -> Result<FileManifest> {
        self.ingest_with(data, &FastCdcChunker::default()).await
    }

    /// Like [`ingest`](Self::ingest), splitting `data` with `chunker`.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache cannot be written.
    pub async fn ingest_with(&self, data: &[u8], chunker: &dyn Chunker) -> Result<FileManifest> {
        let (manifest, chunks) = build_manifest(data, chunker);
        let chunks = chunks
            .into_iter()
            .map(|(id, bytes)| (id, Bytes::copy_from_slice(bytes)))
            .collect();
        self.put_chunks(chunks, None).await?;
        self.put_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Stores `chunks`, several at a time: each is a file of its own, and
    /// creating files is slow on some systems, Windows above all.
    async fn put_chunks(
        &self,
        chunks: Vec<(ChunkId, Bytes)>,
        mut meter: Option<&mut Meter>,
    ) -> Result<()> {
        let mut stored = |len: usize| {
            if let Some(m) = meter.as_deref_mut() {
                m.add(1, len as u64);
            }
        };
        let mut puts = tokio::task::JoinSet::new();
        for (id, data) in chunks {
            if puts.len() >= PARALLEL_PUTS
                && let Some(done) = puts.join_next().await
            {
                stored(done.map_err(join_error)??);
            }
            let store = self.chunks.clone();
            puts.spawn(async move {
                let len = data.len();
                store.put(id, data).await.map(|()| len)
            });
        }
        while let Some(done) = puts.join_next().await {
            stored(done.map_err(join_error)??);
        }
        Ok(())
    }

    /// Returns the bytes of a cached chunk.
    ///
    /// # Errors
    ///
    /// Returns an error if the chunk is missing or corrupt.
    pub async fn chunk(&self, id: ChunkId) -> Result<Bytes> {
        Ok(self.chunks.get(id).await?)
    }

    /// Downloads the manifests and chunks of `contents` that are not cached yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the server lacks any of the content.
    pub async fn fetch(&self, conn: &Connection, contents: &[ContentId]) -> Result<()> {
        let mut missing_manifests = Vec::new();
        let mut manifests = Vec::new();
        for content in contents {
            match self.manifest(content)? {
                Some(m) => manifests.push(m),
                None => missing_manifests.push(*content),
            }
        }
        for batch in missing_manifests.chunks(1024) {
            let Reply::Manifests(found) = conn
                .call(&Request::GetManifests {
                    ids: batch.to_vec(),
                })
                .await?
            else {
                return Err(Error::UnexpectedReply);
            };
            for (id, manifest) in batch.iter().zip(found) {
                let manifest = manifest
                    .ok_or_else(|| Error::Invalid(format!("server has no content {id}")))?;
                if manifest.content != *id || !manifest.is_consistent() {
                    return Err(Error::Invalid(format!(
                        "server sent a bad manifest for {id}"
                    )));
                }
                self.put_manifest(&manifest)?;
                manifests.push(manifest);
            }
        }

        let mut wanted: Vec<(ChunkId, u32)> = manifests
            .iter()
            .flat_map(|m| m.chunks.iter().map(|c| (c.id, c.len)))
            .collect();
        wanted.sort();
        wanted.dedup();
        let ids: Vec<ChunkId> = wanted.iter().map(|(id, _)| *id).collect();
        let have = self.chunks.has(&ids).await?;
        let missing: Vec<(ChunkId, u32)> = wanted
            .into_iter()
            .zip(have)
            .filter(|(_, h)| !h)
            .map(|(c, _)| c)
            .collect();
        let bytes = missing.iter().map(|(_, len)| u64::from(*len)).sum();
        let mut meter = self.meter(Phase::Download, missing.len() as u64, Some(bytes));
        for window in transfer_windows(&missing) {
            let ids: Vec<ChunkId> = window.iter().map(|(id, _)| *id).collect();
            self.put_chunks(conn.get_chunks(&ids).await?, Some(&mut meter))
                .await?;
        }
        Ok(())
    }

    /// Uploads the chunks of `manifests` that the server lacks, then the
    /// manifests themselves.
    ///
    /// # Errors
    ///
    /// Returns an error if a chunk is not cached or the server rejects the
    /// upload.
    pub async fn upload(&self, conn: &Connection, manifests: &[FileManifest]) -> Result<()> {
        let mut wanted: Vec<(ChunkId, u32)> = manifests
            .iter()
            .flat_map(|m| m.chunks.iter().map(|c| (c.id, c.len)))
            .collect();
        wanted.sort();
        wanted.dedup();
        let mut missing = Vec::new();
        for batch in wanted.chunks(QUERY_BATCH) {
            let Reply::Has(has) = conn
                .call(&Request::HasChunks {
                    ids: batch.iter().map(|(id, _)| *id).collect(),
                })
                .await?
            else {
                return Err(Error::UnexpectedReply);
            };
            missing.extend(batch.iter().zip(has).filter(|(_, h)| !h).map(|(c, _)| *c));
        }
        let bytes = missing.iter().map(|(_, len)| u64::from(*len)).sum();
        let mut meter = self.meter(Phase::Upload, missing.len() as u64, Some(bytes));
        for window in transfer_windows(&missing) {
            let mut chunks = Vec::with_capacity(window.len());
            for (id, _) in window {
                chunks.push((*id, self.chunk(*id).await?));
            }
            let size = window.iter().map(|(_, len)| u64::from(*len)).sum();
            conn.put_chunks(chunks).await?;
            meter.add(window.len() as u64, size);
        }
        for batch in manifests.chunks(MANIFEST_BATCH) {
            conn.call(&Request::PutManifests {
                manifests: batch.to_vec(),
            })
            .await?;
        }
        Ok(())
    }

    /// Returns the bytes of a cached file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotCached`] if the content is not fully cached.
    pub async fn read(&self, content: &ContentId) -> Result<Vec<u8>> {
        let manifest = self.manifest(content)?.ok_or(Error::NotCached(*content))?;
        let mut out = Vec::with_capacity(manifest.size as usize);
        for chunk in &manifest.chunks {
            let data = self
                .chunks
                .get(chunk.id)
                .await
                .map_err(|_| Error::NotCached(*content))?;
            out.extend_from_slice(&data);
        }
        Ok(out)
    }

    /// Writes `content` to `dest`, atomically replacing any existing file.
    ///
    /// The file is first assembled in `tmp_dir`, which must be on the same file
    /// system as `dest`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotCached`] if the content is not fully cached, or an
    /// error if writing fails.
    pub async fn materialize(
        &self,
        content: &ContentId,
        executable: bool,
        dest: &Path,
        tmp_dir: &Path,
    ) -> Result<()> {
        let data = self.read(content).await?;
        write_file(&data, executable, dest, tmp_dir)
    }

    fn manifest_path(&self, content: &ContentId) -> PathBuf {
        let hex = content.to_string();
        self.manifests.join(&hex[..2]).join(hex)
    }
}

/// Splits `chunks`, with their lengths, into runs to transfer at a time.
fn transfer_windows(chunks: &[(ChunkId, u32)]) -> Vec<&[(ChunkId, u32)]> {
    let mut windows = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (i, (_, len)) in chunks.iter().enumerate() {
        if i > start && (i - start >= TRANSFER_CHUNKS || size >= TRANSFER_BYTES) {
            windows.push(&chunks[start..i]);
            (start, size) = (i, 0);
        }
        size += u64::from(*len);
    }
    if start < chunks.len() {
        windows.push(&chunks[start..]);
    }
    windows
}

/// Writes `data` to `dest` through a temporary file in `tmp_dir`, replacing
/// any existing file even if it is read-only.
pub(crate) fn write_file(data: &[u8], executable: bool, dest: &Path, tmp_dir: &Path) -> Result<()> {
    fs::create_dir_all(tmp_dir)?;
    let tmp = unique_temp(tmp_dir);
    fs::write(&tmp, data)?;
    crate::fsutil::set_executable(&tmp, executable)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::fsutil::set_read_only(dest, false)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// Returns `true` if the manifest's only chunk is the whole file, so the
/// manifest is implied by the chunk.
pub(crate) fn is_single_chunk(manifest: &FileManifest) -> bool {
    matches!(&manifest.chunks[..], [c] if c.id.0 == manifest.content.0)
}

/// Returns a path in `dir` that no other writer uses at the same time.
pub(crate) fn unique_temp(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".tmp-{}-{n}", std::process::id()))
}
