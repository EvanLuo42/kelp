//! Checking that a server's data is complete: every file any changelist,
//! shelf, review, artifact or plugin refers to can still be read.

use std::collections::HashSet;

use kelp_cas::BlobStore;
use kelp_core::{ChunkId, ContentId};
use kelp_meta::MetaDb;

use crate::Blobs;

const BATCH: usize = 4096;

/// The findings of [`fsck`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    /// The number of audit chain entries verified.
    pub audit_entries: u64,
    /// The number of distinct files that must be readable.
    pub contents: u64,
    /// The number of distinct chunks they consist of.
    pub chunks: u64,
    /// Why the audit chain or a file tree could not be verified, if it
    /// could not.
    pub errors: Vec<String>,
    /// Files whose manifest is missing.
    pub missing_manifests: Vec<ContentId>,
    /// Chunks that are missing from the store.
    pub missing_chunks: Vec<ChunkId>,
    /// Chunks whose bytes do not match their id; only checked deeply.
    pub corrupt_chunks: Vec<ChunkId>,
}

impl FsckReport {
    /// Returns `true` if nothing is missing or corrupt.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
            && self.missing_manifests.is_empty()
            && self.missing_chunks.is_empty()
            && self.corrupt_chunks.is_empty()
    }
}

/// Checks the audit chain, every tree, and that every file still in use has
/// its manifest and chunks. With `deep`, every chunk is read and hashed.
///
/// # Errors
///
/// Returns an error only if the stores cannot be read at all; damage is
/// reported in the [`FsckReport`].
pub async fn fsck(db: &MetaDb, blobs: &Blobs, deep: bool) -> crate::Result<FsckReport> {
    let mut report = FsckReport::default();
    let reader = db.read()?;
    match reader.verify_audit() {
        Ok((entries, _)) => report.audit_entries = entries,
        Err(e) => report.errors.push(format!("audit chain: {e}")),
    }
    if let Err(e) = reader.live_nodes() {
        report.errors.push(format!("trees: {e}"));
    }
    let contents = match reader.live_contents() {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(format!("trees: {e}"));
            HashSet::new()
        }
    };
    report.contents = contents.len() as u64;
    let mut chunks: HashSet<ChunkId> = HashSet::new();
    for content in &contents {
        match reader.manifest(content)? {
            Some(m) => chunks.extend(m.chunks.iter().map(|c| c.id)),
            None => report.missing_manifests.push(*content),
        }
    }
    drop(reader);
    let mut chunks: Vec<ChunkId> = chunks.into_iter().collect();
    chunks.sort();
    report.chunks = chunks.len() as u64;
    for batch in chunks.chunks(BATCH) {
        let present = blobs.has(batch).await?;
        for (id, present) in batch.iter().zip(present) {
            if !present {
                report.missing_chunks.push(*id);
            } else if deep {
                match blobs.get(*id).await {
                    Ok(data) if ChunkId::of(&data) == *id => {}
                    Ok(_) | Err(kelp_cas::Error::Corrupt(_)) => report.corrupt_chunks.push(*id),
                    Err(kelp_cas::Error::NotFound(_)) => report.missing_chunks.push(*id),
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    report.missing_manifests.sort();
    Ok(report)
}
