use std::sync::Arc;

use kelp_core::ClNumber;
use kelp_meta::acl::Level;
use kelp_meta::db::{Outcome, Proposal};
use kelp_meta::record::Artifact;
use kelp_meta::{ArtifactContents, Command, Dirs, FileData};
use kelp_proto::Reply;
use kelp_proto::rpc::{ArtifactInfo, FileInfo};

use super::stream::file_info;
use super::{Access, Response, blocking, internal, invalid, meta_error, not_found, parse_path};
use crate::State;

pub(super) async fn publish(
    state: &Arc<State>,
    access: Access,
    stream: String,
    cl: u64,
    name: String,
    mut tags: Vec<(String, String)>,
    files: Vec<(String, FileInfo)>,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require(
        Level::Write,
        &stream
            .join(&format!(".artifacts/{name}"))
            .map_err(invalid)?,
    )?;
    tags.sort();
    let data = |f: FileInfo| FileData {
        content: f.content,
        size: f.size,
        executable: f.executable,
    };
    let contents = ArtifactContents {
        stream,
        cl: ClNumber(cl),
        name,
        tags,
        files: files.into_iter().map(|(p, f)| (p, data(f))).collect(),
        by: access.name().to_owned(),
        at: state.clock.now(),
    };
    match state.propose(Proposal::PublishArtifact(contents)).await? {
        Outcome::Published { id, .. } => Ok(Reply::Published { id }),
        _ => Err(internal("publishing produced no artifact")),
    }
}

pub(super) fn find(
    state: &State,
    access: &Access,
    stream: String,
    name: String,
    at: Option<u64>,
    tags: Vec<(String, String)>,
    limit: u32,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let reader = state.reader()?;
    let found = reader
        .artifacts(&stream, &name, at.map(ClNumber), &tags)
        .map_err(meta_error)?;
    Ok(Reply::Artifacts(
        found
            .into_iter()
            .take(limit.clamp(1, 1000) as usize)
            .map(info)
            .collect(),
    ))
}

pub(super) fn get(state: &State, access: &Access, id: u64) -> Response {
    let artifact = state
        .reader()?
        .artifact(id)
        .map_err(meta_error)?
        .filter(|a| access.sees(&a.stream))
        .ok_or_else(|| not_found(format!("artifact {id}")))?;
    Ok(Reply::Artifacts(vec![info(artifact)]))
}

pub(super) async fn files(state: &Arc<State>, access: Access, id: u64) -> Response {
    let state = state.clone();
    blocking(move || {
        let reader = state.reader()?;
        let artifact = reader
            .artifact(id)
            .map_err(meta_error)?
            .filter(|a| access.sees(&a.stream));
        let artifact = artifact.ok_or_else(|| not_found(format!("artifact {id}")))?;
        let nodes = reader.nodes().map_err(meta_error)?;
        let files = Dirs::new(&nodes)
            .diff_paths(None, artifact.root)
            .map_err(meta_error)?
            .into_iter()
            .filter_map(|d| file_info(d.new).map(|f| (d.path, f)))
            .collect();
        Ok(Reply::ArtifactFiles(files))
    })
    .await
}

/// Applies a command that manages artifacts, which needs an administrator
/// of the stream they are built from.
pub(super) async fn admin(state: &State, access: &Access, cmd: Command) -> Response {
    let stream = match &cmd {
        Command::PinArtifact { id, .. } | Command::DeleteArtifact { id } => {
            state
                .reader()?
                .artifact(*id)
                .map_err(meta_error)?
                .filter(|a| access.sees(&a.stream))
                .ok_or_else(|| not_found(format!("artifact {id}")))?
                .stream
        }
        Command::SetArtifactRetention { stream, .. } => stream.clone(),
        _ => unreachable!("not an artifact command"),
    };
    access.require_administers(&stream)?;
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn retention(
    state: &State,
    access: &Access,
    stream: String,
    name: String,
    keep_last: Option<u32>,
) -> Response {
    let stream = parse_path(&stream)?;
    admin(
        state,
        access,
        Command::SetArtifactRetention {
            stream,
            name,
            keep_last,
        },
    )
    .await
}

fn info(a: Artifact) -> ArtifactInfo {
    ArtifactInfo {
        id: a.id,
        stream: a.stream.to_string(),
        cl: a.cl.0,
        name: a.name,
        tags: a.tags,
        files: a.files,
        bytes: a.bytes,
        by: a.by,
        at: a.at,
        pinned: a.pinned,
    }
}
