//! Content-addressed storage.
//!
//! Files are split into content-defined chunks addressed by BLAKE3. A file's
//! identity ([`ContentId`](kelp_core::ContentId)) is the hash of its whole
//! content, so the chunking strategy only affects deduplication, never
//! correctness.

pub mod chunker;
pub mod manifest;
pub mod object;
pub mod pack;
pub mod store;
pub mod tiered;

pub use chunker::{ChunkSpan, Chunker, FastCdcChunker};
pub use manifest::build_manifest;
pub use object::{ObjectBlobStore, S3Config};
pub use pack::{Compaction, PACK_FORMAT, PackStore};
pub use store::{BlobStore, FsBlobStore};
pub use tiered::{Demotion, TieredStore};

/// The error type for blob storage operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested chunk is not in the store.
    #[error("chunk {0} not found")]
    NotFound(kelp_core::ChunkId),
    /// The stored chunk could not be decoded or does not match its id.
    #[error("chunk {0} is corrupt")]
    Corrupt(kelp_core::ChunkId),
    /// An underlying I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A specialized [`Result`](std::result::Result) type whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
