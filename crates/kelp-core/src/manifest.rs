//! File manifests describing how content is split into chunks.

use serde::{Deserialize, Serialize};

use crate::{ChunkId, ContentId};

/// A reference to one chunk of a file, in file order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    /// The hash of the uncompressed chunk.
    pub id: ChunkId,
    /// The uncompressed length of the chunk in bytes.
    pub len: u32,
}

/// A mapping from a file's content to the ordered chunks that reconstruct it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifest {
    /// The hash of the whole file.
    pub content: ContentId,
    /// The total file size in bytes.
    pub size: u64,
    /// The chunks that, concatenated in order, reproduce the file.
    pub chunks: Vec<ChunkRef>,
}

impl FileManifest {
    /// Returns `true` if the chunk lengths add up to [`size`](Self::size).
    pub fn is_consistent(&self) -> bool {
        self.chunks.iter().map(|c| c.len as u64).sum::<u64>() == self.size
    }
}
