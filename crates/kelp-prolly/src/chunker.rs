use crate::{Key, Node, NodeId, NodeStore, Result, Value};

/// Node size bounds, in bytes of entry payload.
///
/// A boundary is placed after an entry when the node has reached `max`, or
/// when it is at least `min` and the entry's key hash falls under a threshold
/// that rises linearly from `min` to `max`. The computation uses integer
/// arithmetic only, so every platform agrees on boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkConfig {
    /// The size below which a node is never split.
    pub min: u32,
    /// The size at which a node is always split.
    pub max: u32,
}

impl ChunkConfig {
    /// The default bounds, tuned for directory listings and metadata indexes.
    ///
    /// Nodes are typically 4 to 8 KiB.
    pub const DEFAULT: Self = Self {
        min: 2 << 10,
        max: 16 << 10,
    };

    fn is_boundary(&self, level: u8, key: &[u8], size: usize) -> bool {
        let (min, max) = (self.min as u64, self.max as u64);
        let size = size as u64;
        if size >= max {
            return true;
        }
        if size < min {
            return false;
        }
        let mut h = blake3::Hasher::new();
        h.update(&[level]);
        h.update(key);
        let hash = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap());
        let scaled = ((hash as u128 * (max - min) as u128) >> 64) as u64;
        scaled < size - min
    }
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

pub(crate) fn entry_size(key: &[u8], value: &[u8]) -> usize {
    key.len() + value.len() + 4
}

/// Streams entries of one level into nodes.
pub(crate) struct LevelChunker<'a, S: ?Sized> {
    store: &'a S,
    config: ChunkConfig,
    level: u8,
    buf: Vec<(Key, Value)>,
    size: usize,
    /// The first key and id of every emitted node.
    pub out: Vec<(Key, NodeId)>,
}

impl<'a, S: NodeStore + ?Sized> LevelChunker<'a, S> {
    pub fn new(store: &'a S, config: ChunkConfig, level: u8) -> Self {
        Self {
            store,
            config,
            level,
            buf: Vec::new(),
            size: 0,
            out: Vec::new(),
        }
    }

    pub fn push(&mut self, key: Key, value: Value) -> Result<()> {
        self.size += entry_size(&key, &value);
        let boundary = self.config.is_boundary(self.level, &key, self.size);
        self.buf.push((key, value));
        if boundary {
            self.flush()?;
        }
        Ok(())
    }

    /// Returns `true` if the last pushed entry closed a node.
    pub fn at_boundary(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let entries = std::mem::take(&mut self.buf);
        let first = entries[0].0.clone();
        let id = self.store.put(Node {
            level: self.level,
            entries,
        })?;
        self.out.push((first, id));
        self.size = 0;
        Ok(())
    }
}
