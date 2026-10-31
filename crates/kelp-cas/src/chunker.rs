//! Splitting file content into chunks.

use fastcdc::v2020::FastCDC;

/// A byte range of the input that forms one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSpan {
    /// The offset of the first byte of the chunk.
    pub offset: u64,
    /// The length of the chunk in bytes.
    pub len: u32,
}

/// A strategy for splitting a file into chunks.
///
/// Implementations may be format-aware, such as WASM plugins. They only report
/// boundaries; the host is responsible for hashing.
pub trait Chunker: Send + Sync {
    /// Returns the chunk boundaries of `data`.
    ///
    /// The returned spans must be contiguous, in order, and cover all of `data`.
    /// Empty input yields no spans.
    fn chunk(&self, data: &[u8]) -> Vec<ChunkSpan>;
}

/// Size parameters for FastCDC, in bytes.
#[derive(Clone, Copy, Debug)]
pub struct CdcParams {
    /// The minimum chunk size.
    pub min: u32,
    /// The target average chunk size.
    pub avg: u32,
    /// The maximum chunk size.
    pub max: u32,
}

/// The default content-defined chunker, based on FastCDC.
///
/// Very large files use larger chunks to keep manifests small.
#[derive(Clone, Debug)]
pub struct FastCdcChunker {
    /// The parameters for files smaller than [`large_threshold`](Self::large_threshold).
    pub small: CdcParams,
    /// The parameters for files of at least [`large_threshold`](Self::large_threshold) bytes.
    pub large: CdcParams,
    /// The size in bytes at which [`large`](Self::large) parameters are used.
    pub large_threshold: u64,
    /// The size in bytes below which a file is stored as a single chunk.
    pub inline_threshold: u64,
}

impl Default for FastCdcChunker {
    fn default() -> Self {
        Self {
            small: CdcParams {
                min: 16 << 10,
                avg: 64 << 10,
                max: 256 << 10,
            },
            large: CdcParams {
                min: 256 << 10,
                avg: 1 << 20,
                max: 4 << 20,
            },
            large_threshold: 64 << 20,
            inline_threshold: 64 << 10,
        }
    }
}

impl Chunker for FastCdcChunker {
    fn chunk(&self, data: &[u8]) -> Vec<ChunkSpan> {
        let len = data.len() as u64;
        if len == 0 {
            return Vec::new();
        }
        if len < self.inline_threshold {
            return vec![ChunkSpan {
                offset: 0,
                len: len as u32,
            }];
        }
        let p = if len >= self.large_threshold {
            self.large
        } else {
            self.small
        };
        FastCDC::new(data, p.min, p.avg, p.max)
            .map(|c| ChunkSpan {
                offset: c.offset as u64,
                len: c.length as u32,
            })
            .collect()
    }
}
