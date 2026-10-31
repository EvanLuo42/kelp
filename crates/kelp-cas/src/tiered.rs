//! Hot local storage in front of cold object storage.

use std::collections::HashSet;

use bytes::Bytes;
use kelp_core::ChunkId;

use crate::{BlobStore, Compaction, ObjectBlobStore, PackStore, Result};

/// The result of [`TieredStore::demote`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Demotion {
    /// Chunks moved to the cold tier.
    pub chunks: u64,
    /// Their uncompressed size in bytes.
    pub bytes: u64,
}

/// A [`BlobStore`] that writes to a local hot tier and reads through to a
/// cold object store.
///
/// [`demote`](Self::demote) moves chunks the hot tier no longer needs to the
/// cold tier. A cold chunk that is read again is copied back to the hot tier.
#[derive(Clone)]
pub struct TieredStore {
    hot: PackStore,
    cold: ObjectBlobStore,
}

impl TieredStore {
    /// Combines a hot and a cold tier.
    pub fn new(hot: PackStore, cold: ObjectBlobStore) -> Self {
        Self { hot, cold }
    }

    /// Moves every hot chunk not in `keep_hot` to the cold tier.
    ///
    /// Each chunk is written to the cold tier before it is removed from the
    /// hot one, so an interruption leaves it in both.
    ///
    /// # Errors
    ///
    /// Returns an error if a chunk cannot be copied or removed.
    pub async fn demote(&self, keep_hot: &HashSet<ChunkId>) -> Result<Demotion> {
        let mut result = Demotion::default();
        for id in self.hot.ids().await? {
            if keep_hot.contains(&id) {
                continue;
            }
            let data = self.hot.get(id).await?;
            result.bytes += data.len() as u64;
            self.cold.put(id, data).await?;
            self.hot.delete(id).await?;
            result.chunks += 1;
        }
        Ok(result)
    }

    /// Compacts the hot tier's packs.
    ///
    /// # Errors
    ///
    /// As for [`PackStore::compact`].
    pub async fn compact(&self) -> Result<Compaction> {
        self.hot.compact().await
    }
}

impl BlobStore for TieredStore {
    async fn has(&self, ids: &[ChunkId]) -> Result<Vec<bool>> {
        let mut found = self.hot.has(ids).await?;
        let missing: Vec<(usize, ChunkId)> = found
            .iter()
            .enumerate()
            .filter(|(_, f)| !**f)
            .map(|(i, _)| (i, ids[i]))
            .collect();
        let cold = self
            .cold
            .has(&missing.iter().map(|(_, id)| *id).collect::<Vec<_>>())
            .await?;
        for ((i, _), present) in missing.into_iter().zip(cold) {
            found[i] = present;
        }
        Ok(found)
    }

    async fn get(&self, id: ChunkId) -> Result<Bytes> {
        match self.hot.get(id).await {
            Err(crate::Error::NotFound(_)) => {
                let data = self.cold.get(id).await?;
                self.hot.put(id, data.clone()).await?;
                Ok(data)
            }
            other => other,
        }
    }

    async fn put(&self, id: ChunkId, data: Bytes) -> Result<()> {
        self.hot.put(id, data).await
    }

    async fn delete(&self, id: ChunkId) -> Result<()> {
        self.hot.delete(id).await?;
        self.cold.delete(id).await
    }

    async fn sync(&self) -> Result<()> {
        self.hot.sync().await
    }

    async fn ids(&self) -> Result<Vec<ChunkId>> {
        let mut ids: HashSet<ChunkId> = self.hot.ids().await?.into_iter().collect();
        ids.extend(self.cold.ids().await?);
        Ok(ids.into_iter().collect())
    }
}
