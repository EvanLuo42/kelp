//! Where the server keeps chunks, chosen in `<data>/server.toml`.

use std::collections::HashSet;
use std::path::Path;

use bytes::Bytes;
use kelp_cas::{
    BlobStore, Compaction, Demotion, ObjectBlobStore, PackStore, S3Config, TieredStore,
};
use kelp_core::ChunkId;
use serde::Deserialize;

use crate::{Error, Result};

/// The `[storage]` section of `server.toml`.
///
/// ```toml
/// [storage]
/// kind = "tiered"          # "local" (default), "s3" or "tiered"
///
/// [storage.s3]
/// bucket = "kelp-chunks"
/// region = "eu-central-1"
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct StorageConfig {
    #[serde(default)]
    pub kind: StorageKind,
    pub s3: Option<S3Config>,
}

/// How chunks are stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    /// Packs and loose files in the data directory.
    #[default]
    Local,
    /// Objects in an S3-compatible bucket.
    S3,
    /// Local storage for current content, the bucket for history.
    Tiered,
}

#[derive(Deserialize)]
struct ServerToml {
    #[serde(default)]
    storage: StorageConfig,
}

impl StorageConfig {
    /// Reads the storage section of `<data_dir>/server.toml`, or returns the
    /// default if the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(data_dir: &Path) -> Result<Self> {
        match std::fs::read_to_string(data_dir.join("server.toml")) {
            Ok(text) => toml::from_str::<ServerToml>(&text)
                .map(|t| t.storage)
                .map_err(|e| Error::Config(format!("server.toml: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}

/// The server's chunk store.
#[derive(Clone)]
pub enum Blobs {
    Local(PackStore),
    S3(ObjectBlobStore),
    Tiered(TieredStore),
}

impl Blobs {
    /// Opens the store `config` describes, keeping local data in `dir`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if S3 settings are missing, or an error if
    /// a store cannot be opened.
    pub fn open(config: &StorageConfig, dir: &Path) -> Result<Self> {
        let s3 = || -> Result<ObjectBlobStore> {
            let s3 = config
                .s3
                .as_ref()
                .ok_or_else(|| Error::Config("[storage.s3] is missing".into()))?;
            ObjectBlobStore::s3(s3).map_err(|e| Error::Config(e.to_string()))
        };
        let local = || PackStore::open(dir).map_err(|e| Error::Io(std::io::Error::other(e)));
        Ok(match config.kind {
            StorageKind::Local => Self::Local(local()?),
            StorageKind::S3 => Self::S3(s3()?),
            StorageKind::Tiered => Self::Tiered(TieredStore::new(local()?, s3()?)),
        })
    }

    /// Compacts local packs, if there are any.
    ///
    /// # Errors
    ///
    /// As for [`PackStore::compact`].
    pub async fn compact(&self) -> kelp_cas::Result<Compaction> {
        match self {
            Self::Local(s) => s.compact().await,
            Self::S3(_) => Ok(Compaction::default()),
            Self::Tiered(s) => s.compact().await,
        }
    }

    /// Moves chunks not in `keep_hot` to the cold tier, or returns `None` if
    /// the storage is not tiered.
    ///
    /// # Errors
    ///
    /// As for [`TieredStore::demote`].
    pub async fn demote(&self, keep_hot: &HashSet<ChunkId>) -> kelp_cas::Result<Option<Demotion>> {
        match self {
            Self::Tiered(s) => s.demote(keep_hot).await.map(Some),
            _ => Ok(None),
        }
    }
}

macro_rules! delegate {
    ($self:ident, $s:ident => $call:expr) => {
        match $self {
            Self::Local($s) => $call.await,
            Self::S3($s) => $call.await,
            Self::Tiered($s) => $call.await,
        }
    };
}

impl BlobStore for Blobs {
    async fn has(&self, ids: &[ChunkId]) -> kelp_cas::Result<Vec<bool>> {
        delegate!(self, s => s.has(ids))
    }

    async fn get(&self, id: ChunkId) -> kelp_cas::Result<Bytes> {
        delegate!(self, s => s.get(id))
    }

    async fn put(&self, id: ChunkId, data: Bytes) -> kelp_cas::Result<()> {
        delegate!(self, s => s.put(id, data))
    }

    async fn delete(&self, id: ChunkId) -> kelp_cas::Result<()> {
        delegate!(self, s => s.delete(id))
    }

    async fn sync(&self) -> kelp_cas::Result<()> {
        delegate!(self, s => s.sync())
    }

    async fn ids(&self) -> kelp_cas::Result<Vec<ChunkId>> {
        delegate!(self, s => s.ids())
    }
}
