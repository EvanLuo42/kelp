//! A chunk store on an object store such as S3.

use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use kelp_core::ChunkId;
use object_store::path::Path;
use object_store::{ObjectStore, PutPayload};

use crate::store::{compress, decompress, parse_id};
use crate::{BlobStore, Error, Result};

const PREFIX: &str = "chunks";
const LEVEL: i32 = 3;
const PARALLEL_REQUESTS: usize = 32;

/// Settings for an S3-compatible bucket.
///
/// Credentials not given here are read from the standard AWS environment
/// variables, such as `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    pub bucket: String,
    /// A custom endpoint for S3-compatible services such as MinIO or R2.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Allows plain HTTP endpoints, for local test servers.
    #[serde(default)]
    pub allow_http: bool,
    /// A key prefix under which the store keeps its objects, so several
    /// servers can share a bucket.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// A [`BlobStore`] keeping each chunk as a zstd-compressed object under
/// `chunks/ab/cd/<hex>`.
#[derive(Clone)]
pub struct ObjectBlobStore {
    store: Arc<dyn ObjectStore>,
}

impl ObjectBlobStore {
    /// Creates a store on any object store, for example an in-memory one.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Connects to an S3-compatible bucket.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the configuration is incomplete.
    pub fn s3(config: &S3Config) -> Result<Self> {
        let mut builder = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(&config.bucket)
            .with_allow_http(config.allow_http);
        if let Some(endpoint) = &config.endpoint {
            builder = builder.with_endpoint(endpoint);
        }
        if let Some(region) = &config.region {
            builder = builder.with_region(region);
        }
        let s3 = builder.build().map_err(io)?;
        Ok(Self::new(match &config.prefix {
            Some(prefix) => Arc::new(object_store::prefix::PrefixStore::new(s3, prefix.as_str())),
            None => Arc::new(s3),
        }))
    }

    fn path(id: ChunkId) -> Path {
        let hex = id.to_string();
        Path::from(format!("{PREFIX}/{}/{}/{hex}", &hex[0..2], &hex[2..4]))
    }

    async fn exists(&self, id: ChunkId) -> Result<bool> {
        match self.store.head(&Self::path(id)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(io(e)),
        }
    }
}

impl BlobStore for ObjectBlobStore {
    async fn has(&self, ids: &[ChunkId]) -> Result<Vec<bool>> {
        let lookups = ids.iter().copied().map(|id| {
            let store = self.clone();
            async move { store.exists(id).await }
        });
        futures::stream::iter(lookups)
            .buffered(PARALLEL_REQUESTS)
            .try_collect()
            .await
    }

    async fn get(&self, id: ChunkId) -> Result<Bytes> {
        let compressed = match self.store.get(&Self::path(id)).await {
            Ok(result) => result.bytes().await.map_err(io)?,
            Err(object_store::Error::NotFound { .. }) => return Err(Error::NotFound(id)),
            Err(e) => return Err(io(e)),
        };
        let data = decompress(&compressed).map_err(|_| Error::Corrupt(id))?;
        if ChunkId::of(&data) != id {
            return Err(Error::Corrupt(id));
        }
        Ok(data.into())
    }

    async fn put(&self, id: ChunkId, data: Bytes) -> Result<()> {
        let compressed = tokio::task::spawn_blocking(move || compress(&data, LEVEL))
            .await
            .map_err(io)??;
        self.store
            .put(&Self::path(id), PutPayload::from(compressed))
            .await
            .map_err(io)?;
        Ok(())
    }

    async fn delete(&self, id: ChunkId) -> Result<()> {
        match self.store.delete(&Self::path(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(io(e)),
        }
    }

    async fn ids(&self) -> Result<Vec<ChunkId>> {
        let prefix = Path::from(PREFIX);
        let mut ids = Vec::new();
        let mut listing = self.store.list(Some(&prefix));
        while let Some(meta) = listing.next().await {
            if let Some(id) = meta.map_err(io)?.location.filename().and_then(parse_id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}
