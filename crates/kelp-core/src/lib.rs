//! Core types shared by every Kelp component.
//!
//! This crate performs no IO. It defines identifiers, depot path semantics
//! (case-insensitive, case-preserving) and the typemap that drives per-file
//! behaviour such as locking, retention and chunking.

pub mod error;
pub mod id;
pub mod issue;
pub mod manifest;
pub mod path;
pub mod time;
pub mod typemap;

pub use error::{Error, Result};
pub use id::{ChunkId, ClNumber, ContentId, Hash, LocalClId, TreeId};
pub use manifest::{ChunkRef, FileManifest};
pub use path::DepotPath;
