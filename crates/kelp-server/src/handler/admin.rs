use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kelp_cas::BlobStore;
use kelp_core::{ChunkId, ContentId, Hash};
use kelp_meta::Command;
use kelp_meta::db::{Outcome, Proposal};
use kelp_proto::rpc::{AuditInfo, FsckInfo, GcReport, LogEntry};
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::{Access, Response, blocking, internal, meta_error};
use crate::State;

const MAX_LOG_BATCH: u32 = 10_000;
const MAX_LOG_WAIT_MS: u32 = 30_000;
const LOG_POLL: Duration = Duration::from_millis(10);

/// Recently used chunk and content ids, which garbage collection keeps.
///
/// A client checks or uploads content before submitting it; until the submit
/// commits, nothing else refers to that content.
pub struct Leases {
    duration: Duration,
    touched: Mutex<HashMap<Hash, u64>>,
}

impl Leases {
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            touched: Mutex::default(),
        }
    }

    /// Records that `ids` were used at `now` (milliseconds).
    pub fn touch(&self, ids: impl IntoIterator<Item = Hash>, now: u64) {
        let mut touched = self.touched.lock().expect("lease lock");
        for id in ids {
            touched.insert(id, now);
        }
    }

    /// Returns the ids used within the lease duration, forgetting older ones.
    pub(crate) fn current(&self, now: u64) -> HashSet<Hash> {
        let cutoff = now.saturating_sub(self.duration.as_millis() as u64);
        let mut touched = self.touched.lock().expect("lease lock");
        touched.retain(|_, at| *at >= cutoff);
        touched.keys().copied().collect()
    }
}

pub(super) async fn gc(state: &Arc<State>, access: &Access, dry_run: bool) -> Response {
    access.require_admin()?;
    let lease = state.leases.duration.as_millis() as u64;
    // A new leader does not know what was uploaded through its predecessor.
    let since = match state.cluster() {
        Some(c) => c.leader_since().unwrap_or(u64::MAX),
        None => 0,
    };
    if state.clock.now() < state.started_at.max(since).saturating_add(lease) {
        return Err(ErrorReply::new(
            ErrorKind::Conflict,
            "garbage collection waits one lease period after start-up, so uploads from before a restart are not lost",
        ));
    }
    let _exclusive = state.gc_lock.lock().await;
    let leased = state.leases.current(state.clock.now());
    let stored = state.blobs.ids().await.map_err(internal)?;

    let marking = state.clone();
    let (live_chunks, dead_manifests) = blocking(move || {
        let reader = marking.reader()?;
        let mut contents = reader.live_contents().map_err(meta_error)?;
        contents.extend(leased.iter().map(|h| ContentId(*h)));
        let mut chunks: HashSet<ChunkId> = leased.iter().map(|h| ChunkId(*h)).collect();
        for content in &contents {
            chunks.insert(ChunkId(content.0));
            if let Some(manifest) = reader.manifest(content).map_err(meta_error)? {
                chunks.extend(manifest.chunks.iter().map(|c| c.id));
            }
        }
        let dead_manifests: Vec<ContentId> = reader
            .manifest_ids()
            .map_err(meta_error)?
            .into_iter()
            .filter(|c| !contents.contains(c))
            .collect();
        Ok((chunks, dead_manifests))
    })
    .await?;

    let mut dead_chunks: Vec<ChunkId> = stored
        .iter()
        .filter(|id| !live_chunks.contains(id))
        .copied()
        .collect();
    let mut dead_manifests = dead_manifests;
    let mut report = GcReport {
        dry_run,
        chunks_kept: (stored.len() - dead_chunks.len()) as u64,
        chunks_deleted: dead_chunks.len() as u64,
        manifests_deleted: dead_manifests.len() as u64,
        ..GcReport::default()
    };
    if dry_run {
        return Ok(Reply::Gc(report));
    }
    if let Some(pause) = &state.gc_pause {
        pause.marked.notify_one();
        pause.resume.notified().await;
    }
    // Uploads wait while content is deleted. Anything a client asked about
    // or uploaded since the leases were read above may be about to be
    // submitted, so it is spared.
    let _fence = state.sweep.write().await;
    let leased = state.leases.current(state.clock.now());
    // A submit leases the content it refers to, not the chunks; spare
    // the chunks of leased content too.
    let mut spared: HashSet<Hash> = leased.clone();
    {
        let reader = state.reader()?;
        for id in &leased {
            if let Some(manifest) = reader.manifest(&ContentId(*id)).map_err(meta_error)? {
                spared.extend(manifest.chunks.iter().map(|c| c.id.0));
            }
        }
    }
    dead_chunks.retain(|id| !spared.contains(&id.0));
    dead_manifests.retain(|c| !leased.contains(&c.0));
    report.chunks_kept = (stored.len() - dead_chunks.len()) as u64;
    report.chunks_deleted = dead_chunks.len() as u64;
    report.manifests_deleted = dead_manifests.len() as u64;
    report.nodes_deleted = match state.propose(Proposal::Collect { dead_manifests }).await? {
        Outcome::Collected { nodes } => nodes,
        _ => 0,
    };
    for id in dead_chunks {
        state.blobs.delete(id).await.map_err(internal)?;
    }
    let compaction = state.blobs.compact().await.map_err(internal)?;
    report.packs_compacted = compaction.packs as u64;
    report.bytes_freed = compaction.freed;
    Ok(Reply::Gc(report))
}

pub(super) async fn obliterate(state: &Arc<State>, access: Access, paths: Vec<String>) -> Response {
    access.require_admin()?;
    if paths.is_empty() {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "name at least one path to obliterate",
        ));
    }
    // Obliterate rewrites old trees, which a collection in progress would
    // not know to keep.
    let _exclusive = state.gc_lock.lock().await;
    state
        .commit(access.name(), Command::Obliterate { paths })
        .await?;
    Ok(Reply::Ok)
}

pub(super) fn audit_log(state: &State, access: &Access, from: u64, limit: u32) -> Response {
    access.require_admin()?;
    let entries = state
        .reader()?
        .audit(from, limit.min(10_000) as usize)
        .map_err(meta_error)?;
    Ok(Reply::Audit(
        entries
            .into_iter()
            .map(|e| AuditInfo {
                index: e.index,
                actor: e.actor,
                summary: e.summary,
                hash: e.hash.to_string(),
            })
            .collect(),
    ))
}

pub(super) async fn backup(
    state: &Arc<State>,
    access: &Access,
    dest: String,
    metadata_only: bool,
) -> Response {
    access.require_admin()?;
    // Collection must not delete chunks the snapshot still needs.
    let _exclusive = state.gc_lock.lock().await;
    let report = crate::backup::backup(
        &state.db,
        &state.blobs,
        &state.data_dir,
        std::path::Path::new(&dest),
        metadata_only,
    )
    .await
    .map_err(internal)?;
    Ok(Reply::BackedUp {
        log_index: report.log_index,
        chunks_copied: report.chunks_copied,
        chunks_present: report.chunks_present,
        bytes_copied: report.bytes_copied,
    })
}

pub(super) async fn fsck(state: &Arc<State>, access: &Access, deep: bool) -> Response {
    access.require_admin()?;
    let _exclusive = state.gc_lock.lock().await;
    let r = crate::fsck::fsck(&state.db, &state.blobs, deep)
        .await
        .map_err(internal)?;
    Ok(Reply::Fsck(FsckInfo {
        audit_entries: r.audit_entries,
        contents: r.contents,
        chunks: r.chunks,
        errors: r.errors,
        missing_manifests: r.missing_manifests,
        missing_chunks: r.missing_chunks,
        corrupt_chunks: r.corrupt_chunks,
    }))
}

pub(super) async fn read_log(
    state: &Arc<State>,
    access: &Access,
    from: u64,
    limit: u32,
    wait_ms: u32,
) -> Response {
    if !(access.is_replica() || access.is_admin()) {
        return Err(ErrorReply::new(
            ErrorKind::Forbidden,
            "reading the log requires an edge account",
        ));
    }
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(wait_ms.min(MAX_LOG_WAIT_MS).into());
    loop {
        let reading = state.clone();
        let (entries, head, digest) = blocking(move || {
            let reader = reading.reader()?;
            let entries = match limit {
                0 => Vec::new(),
                _ => reader
                    .log_entries(from, limit.min(MAX_LOG_BATCH) as usize)
                    .map_err(meta_error)?,
            };
            let head = reader.last_index().map_err(internal)?.0;
            Ok((entries, head, reader.stream_digest().map_err(meta_error)?))
        })
        .await?;
        if !entries.is_empty() || limit == 0 || tokio::time::Instant::now() >= deadline {
            let entries = entries
                .into_iter()
                .map(|e| LogEntry {
                    index: e.index,
                    actor: e.actor,
                    command: e.command,
                    hash: e.hash,
                })
                .collect();
            return Ok(Reply::Log {
                entries,
                head,
                format: kelp_meta::db::FORMAT_VERSION,
                digest,
            });
        }
        tokio::time::sleep(LOG_POLL).await;
    }
}

pub(super) async fn verify_audit(state: &Arc<State>, access: &Access) -> Response {
    access.require_admin()?;
    let state = state.clone();
    blocking(move || {
        let (entries, head) = state.reader()?.verify_audit().map_err(meta_error)?;
        Ok(Reply::AuditVerified {
            entries,
            head: head.to_string(),
        })
    })
    .await
}

pub(super) async fn tier(state: &Arc<State>, access: &Access) -> Response {
    access.require_admin()?;
    // Demoted chunks stay readable, so unlike collection this needs no leases.
    let _exclusive = state.gc_lock.lock().await;
    let marking = state.clone();
    let keep = blocking(move || {
        let reader = marking.reader()?;
        let mut chunks = HashSet::new();
        for content in reader.head_contents().map_err(meta_error)? {
            chunks.insert(ChunkId(content.0));
            if let Some(manifest) = reader.manifest(&content).map_err(meta_error)? {
                chunks.extend(manifest.chunks.iter().map(|c| c.id));
            }
        }
        Ok(chunks)
    })
    .await?;
    match state.blobs.demote(&keep).await.map_err(internal)? {
        Some(d) => Ok(Reply::Tiered {
            chunks: d.chunks,
            bytes: d.bytes,
        }),
        None => Err(ErrorReply::new(
            ErrorKind::Invalid,
            "storage is not tiered; set [storage] kind = \"tiered\"",
        )),
    }
}
