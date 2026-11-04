use std::sync::Arc;

use bytes::Bytes;
use kelp_cas::BlobStore;
use kelp_core::{ChunkId, ContentId, FileManifest};
use kelp_meta::db::Proposal;
use kelp_proto::io::{expect_frame, split_chunk, write_chunk};
use kelp_proto::{ErrorKind, ErrorReply, MsgType, Reply};
use quinn::{RecvStream, SendStream};
use tokio::task::JoinSet;

use super::{Access, Response, check_batch, internal, invalid, meta_error};
use crate::State;

const PARALLEL_WRITES: usize = 16;

pub(super) async fn has_chunks(state: &State, ids: Vec<ChunkId>) -> Response {
    check_batch(ids.len())?;
    let _fence = state.sweep.read().await;
    state
        .leases
        .touch(ids.iter().map(|id| id.0), state.clock.now());
    Ok(Reply::Has(state.blobs.has(&ids).await.map_err(internal)?))
}

pub(super) async fn put_chunks(state: &Arc<State>, count: u32, recv: &mut RecvStream) -> Response {
    check_batch(count as usize)?;
    let _fence = state.sweep.read().await;
    let mut writes: JoinSet<kelp_cas::Result<()>> = JoinSet::new();
    for _ in 0..count {
        let frame = expect_frame(recv).await.map_err(invalid)?;
        frame.expect(MsgType::Chunk).map_err(invalid)?;
        let (id, data) = split_chunk(&frame.payload).map_err(invalid)?;
        let id = ChunkId(kelp_core::Hash(id));
        state.leases.touch([id.0], state.clock.now());
        if ChunkId::of(data) != id {
            return Err(ErrorReply::new(
                ErrorKind::Invalid,
                format!("chunk {id} does not match its id"),
            ));
        }
        if writes.len() >= PARALLEL_WRITES {
            writes
                .join_next()
                .await
                .expect("writes are pending")
                .map_err(internal)?
                .map_err(internal)?;
        }
        let (state, data) = (state.clone(), Bytes::copy_from_slice(data));
        writes.spawn(async move { state.blobs.put(id, data).await });
    }
    while let Some(done) = writes.join_next().await {
        done.map_err(internal)?.map_err(internal)?;
    }
    state.blobs.sync().await.map_err(internal)?;
    Ok(Reply::Ok)
}

/// Sends the chunks `ids`.
///
/// Chunks are not checked against protections: chunk ids are BLAKE3 hashes,
/// which act as capabilities, and a user learns them only from manifests,
/// which [`get_manifests`] hands out only for content the user may read.
pub(super) async fn get_chunks(
    state: &State,
    ids: Vec<ChunkId>,
    send: &mut SendStream,
) -> Response {
    check_batch(ids.len())?;
    for id in ids {
        let data = state.blobs.get(id).await.map_err(|e| match e {
            kelp_cas::Error::NotFound(_) => ErrorReply::new(ErrorKind::NotFound, e.to_string()),
            e => internal(e),
        })?;
        write_chunk(send, &id.0.0, &data).await.map_err(internal)?;
    }
    Ok(Reply::Ok)
}

pub(super) async fn put_manifests(state: &State, manifests: Vec<FileManifest>) -> Response {
    check_batch(manifests.len())?;
    let _fence = state.sweep.read().await;
    let now = state.clock.now();
    let reader = state.reader()?;
    let mut new = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        state.leases.touch(
            [manifest.content.0]
                .into_iter()
                .chain(manifest.chunks.iter().map(|c| c.id.0)),
            now,
        );
        // A stored manifest was verified when it was written; never replace
        // it with another claim about the same content.
        if reader
            .manifest(&manifest.content)
            .map_err(internal)?
            .is_none()
        {
            verify_manifest(state, &manifest).await?;
            new.push(manifest);
        }
    }
    drop(reader);
    // The manifests must not outlive a crash that loses their chunks.
    state.blobs.sync().await.map_err(internal)?;
    if !new.is_empty() {
        state.propose(Proposal::PutManifests(new)).await?;
    }
    Ok(Reply::Ok)
}

/// Returns the manifests of `ids`, leaving out content the user may not
/// read as if the server lacked it.
pub(super) fn get_manifests(state: &State, access: &Access, ids: Vec<ContentId>) -> Response {
    check_batch(ids.len())?;
    let reader = state.reader()?;
    let mut manifests = Vec::with_capacity(ids.len());
    for id in &ids {
        let manifest = match access.reads_content(&reader, id)? {
            true => reader.manifest(id).map_err(meta_error)?,
            false => None,
        };
        manifests.push(manifest);
    }
    Ok(Reply::Manifests(manifests))
}

/// Refuses chunks named by their ids alone to a user who may not read
/// everything, since the server cannot tell what content they belong to.
pub(super) fn require_reads_everything(access: &Access) -> Result<(), ErrorReply> {
    match access.reads_everything() {
        true => Ok(()),
        false => Err(ErrorReply::new(
            ErrorKind::Forbidden,
            "this server checks read permission on content; upgrade Kelp to download it",
        )),
    }
}

/// Checks that each of `ids` is a chunk of one of `contents` and that the
/// user may read that content.
pub(super) fn check_content_chunks(
    state: &State,
    access: &Access,
    contents: &[ContentId],
    ids: &[ChunkId],
) -> Result<(), ErrorReply> {
    if access.reads_everything() {
        return Ok(());
    }
    let reader = state.reader()?;
    let mut allowed = std::collections::HashSet::new();
    for content in contents {
        let manifest = match access.reads_content(&reader, content)? {
            true => reader.manifest(content).map_err(meta_error)?,
            false => None,
        };
        let manifest = manifest.ok_or_else(|| super::not_found(format!("content {content}")))?;
        allowed.extend(manifest.chunks.iter().map(|c| c.id));
    }
    match ids.iter().find(|id| !allowed.contains(id)) {
        None => Ok(()),
        Some(id) => Err(ErrorReply::new(
            ErrorKind::Forbidden,
            format!("chunk {id} is not part of the content named with it"),
        )),
    }
}

async fn verify_manifest(state: &State, manifest: &FileManifest) -> Result<(), ErrorReply> {
    if !manifest.is_consistent() {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "manifest chunk lengths do not add up to its size",
        ));
    }
    // A single chunk whose id is the content id was hashed when it was uploaded.
    if let [chunk] = &manifest.chunks[..]
        && chunk.id.0 == manifest.content.0
    {
        let data = state.blobs.get(chunk.id).await.map_err(|_| {
            ErrorReply::new(
                ErrorKind::Invalid,
                format!("chunk {} of manifest is not uploaded", chunk.id),
            )
        })?;
        return match data.len() as u64 == manifest.size {
            true => Ok(()),
            false => Err(ErrorReply::new(
                ErrorKind::Invalid,
                format!("chunk {} has the wrong length", chunk.id),
            )),
        };
    }
    let mut hasher = blake3::Hasher::new();
    for chunk in &manifest.chunks {
        let data = state.blobs.get(chunk.id).await.map_err(|_| {
            ErrorReply::new(
                ErrorKind::Invalid,
                format!("chunk {} of manifest is not uploaded", chunk.id),
            )
        })?;
        if data.len() != chunk.len as usize {
            return Err(ErrorReply::new(
                ErrorKind::Invalid,
                format!("chunk {} has the wrong length", chunk.id),
            ));
        }
        hasher.update(&data);
    }
    if *hasher.finalize().as_bytes() != manifest.content.0.0 {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "manifest chunks do not hash to its content id",
        ));
    }
    Ok(())
}
