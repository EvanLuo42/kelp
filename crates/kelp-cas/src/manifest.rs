//! Building file manifests from raw content.

use kelp_core::{ChunkId, ChunkRef, ContentId, FileManifest};

use crate::Chunker;

/// Chunks `data` and returns its manifest along with the borrowed chunk payloads.
///
/// The payloads are returned in file order, paired with their ids, so the
/// caller can upload them without re-slicing `data`.
pub fn build_manifest<'a>(
    data: &'a [u8],
    chunker: &dyn Chunker,
) -> (FileManifest, Vec<(ChunkId, &'a [u8])>) {
    let payloads: Vec<_> = chunker
        .chunk(data)
        .into_iter()
        .map(|s| {
            let bytes = &data[s.offset as usize..][..s.len as usize];
            (ChunkId::of(bytes), bytes)
        })
        .collect();
    let manifest = FileManifest {
        content: ContentId::of(data),
        size: data.len() as u64,
        chunks: payloads
            .iter()
            .map(|(id, b)| ChunkRef {
                id: *id,
                len: b.len() as u32,
            })
            .collect(),
    };
    (manifest, payloads)
}
