use std::sync::Arc;

use kelp_core::ClNumber;
use kelp_meta::acl::Level;
use kelp_meta::db::Proposal;
use kelp_meta::record::{Import, Stream};
use kelp_meta::{Command, Dirs, EntryKind, StreamKind};
use kelp_proto::rpc::{FileDelta, FileInfo, ImportInfo, IntegrationInfo, StreamInfo};
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::{Access, Response, blocking, meta_error, not_found, parse_path};
use crate::State;

pub(super) async fn create_depot(state: &State, access: &Access, name: String) -> Response {
    access.require_administers(&parse_path(&format!("//{name}"))?)?;
    let at = state.clock.now();
    let main = Command::CreateStream {
        path: parse_path(&format!("//{name}/main"))?,
        kind: StreamKind::Mainline,
        parent: None,
        at,
    };
    let depot = Command::CreateDepot { name, at };
    state.commit_all(access.name(), vec![depot, main]).await?;
    Ok(Reply::Ok)
}

pub(super) fn get_typemap(state: &State, depot: String) -> Response {
    let record = state.reader()?.typemap(&depot).map_err(meta_error)?;
    Ok(Reply::Text(record.map(|r| r.source)))
}

pub(super) async fn set_typemap(
    state: &State,
    access: &Access,
    depot: String,
    source: String,
) -> Response {
    access.require_administers(&parse_path(&format!("//{depot}"))?)?;
    let cmd = Command::SetTypemap {
        depot,
        source,
        by: access.name().to_owned(),
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) fn get_imports(state: &State, access: &Access, stream: String) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let imports = state.reader()?.imports(&stream).map_err(meta_error)?;
    Ok(Reply::Imports(
        imports
            .into_iter()
            .map(|i| ImportInfo {
                source: i.source.to_string(),
                from: i.from,
                to: i.to,
                at: i.at.map(|c| c.0),
                writable: i.writable,
            })
            .collect(),
    ))
}

pub(super) async fn set_imports(
    state: &State,
    access: &Access,
    stream: String,
    imports: Vec<ImportInfo>,
) -> Response {
    let target = parse_path(&stream)?;
    access.require_administers(&target)?;
    let imports = imports
        .into_iter()
        .map(|i| {
            Ok(Import {
                source: {
                    let source = parse_path(&i.source)?;
                    access.require_stream(&source)?;
                    source
                },
                from: i.from,
                to: i.to,
                at: i.at.map(ClNumber),
                writable: i.writable,
            })
        })
        .collect::<Result<_, ErrorReply>>()?;
    let cmd = Command::SetImports {
        stream: target,
        imports,
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn delete_stream(state: &State, access: &Access, stream: String) -> Response {
    let path = parse_path(&stream)?;
    access.require_stream(&path)?;
    let record = state
        .reader()?
        .stream(&path)
        .map_err(meta_error)?
        .ok_or_else(|| not_found(format!("stream {path}")))?;
    if record.kind != StreamKind::Task {
        access.require_administers(&path)?;
    }
    state
        .commit(access.name(), Command::DeleteStream { path })
        .await?;
    Ok(Reply::Ok)
}

/// Creates a stream. Administrators of the stream create any kind; other users who can
/// see the parent may create task streams.
pub(super) async fn create_stream(
    state: &State,
    access: &Access,
    path: String,
    kind: kelp_proto::rpc::StreamKind,
    parent: Option<String>,
) -> Response {
    let kind = meta_kind(kind);
    let parent = parent.as_deref().map(parse_path).transpose()?;
    let path = parse_path(&path)?;
    if !access.administers(&path) {
        match (&parent, kind) {
            (Some(p), StreamKind::Task) => access.require_stream(p)?,
            _ => {
                return Err(ErrorReply::new(
                    ErrorKind::Forbidden,
                    format!("only administrators of {path} create this kind of stream"),
                ));
            }
        }
    }
    let cmd = Command::CreateStream {
        path,
        kind,
        parent,
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) fn list(state: &State, access: &Access) -> Response {
    let streams = state.reader()?.streams().map_err(meta_error)?;
    Ok(Reply::Streams(
        streams
            .into_iter()
            .filter(|s| access.sees(&s.path))
            .map(info)
            .collect(),
    ))
}

pub(super) fn resolve(state: &State, access: &Access, stream: String, at: Option<u64>) -> Response {
    let path = parse_path(&stream)?;
    access.require_stream(&path)?;
    let reader = state.reader()?;
    let record = reader.stream(&path).map_err(meta_error)?;
    let record = record.ok_or_else(|| not_found(format!("stream {path}")))?;
    let (cl, root) = reader
        .view_at(&path, at.map(ClNumber))
        .map_err(meta_error)?;
    Ok(Reply::Stream(StreamInfo {
        cl: cl.0,
        root,
        ..info(record)
    }))
}

pub(super) async fn diff_tree(
    state: &Arc<State>,
    access: Access,
    stream: String,
    from: Option<kelp_core::TreeId>,
    to: kelp_core::TreeId,
    paths: Vec<String>,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let state = state.clone();
    blocking(move || {
        let reader = state.reader()?;
        let real_paths = reader.real_paths().map_err(meta_error)?;
        let readable = |rel: &str| {
            stream.join(rel).is_ok_and(|p| {
                real_paths
                    .real_path(&p)
                    .is_ok_and(|real| access.can(Level::Read, &p) && access.can(Level::Read, &real))
            })
        };
        let nodes = reader.nodes().map_err(meta_error)?;
        let dirs = Dirs::new(&nodes);
        let changes = dirs
            .diff_paths(from, to)
            .map_err(meta_error)?
            .into_iter()
            .filter(|d| readable(&d.path))
            .map(|d| FileDelta {
                path: d.path,
                old: file_info(d.old),
                new: file_info(d.new),
            })
            .filter(|d| d.old.is_some() || d.new.is_some())
            .collect();
        let lookups = paths
            .iter()
            .map(|p| match readable(p) {
                true => Ok(file_info(
                    dirs.lookup(to, p).map_err(meta_error)?.map(|e| e.kind),
                )),
                false => Ok(None),
            })
            .collect::<Result<_, ErrorReply>>()?;
        Ok(Reply::TreeDiff { changes, lookups })
    })
    .await
}

pub(super) fn integration(
    state: &State,
    access: &Access,
    target: String,
    source: String,
) -> Response {
    let (target, source) = (parse_path(&target)?, parse_path(&source)?);
    access.require_stream(&target)?;
    access.require_stream(&source)?;
    let reader = state.reader()?;
    let head = reader.stream(&source).map_err(meta_error)?;
    let head = head.ok_or_else(|| not_found(format!("stream {source}")))?;
    let base_cl = reader
        .integration(&target, &source)
        .map_err(meta_error)?
        .unwrap_or(ClNumber(0));
    let (_, base_root) = reader
        .stream_at(&source, Some(base_cl))
        .map_err(meta_error)?;
    Ok(Reply::Integration(IntegrationInfo {
        base_cl: base_cl.0,
        base_root,
        source_cl: head.head.0,
        source_root: head.root,
    }))
}

pub(super) async fn copy_up(
    state: &Arc<State>,
    access: Access,
    stream: String,
    description: String,
) -> Response {
    let child = parse_path(&stream)?;
    access.require_stream(&child)?;
    let checking = state.clone();
    let (child, access) = blocking(move || {
        {
            let reader = checking.reader()?;
            let record = reader.stream(&child).map_err(meta_error)?;
            let record = record.ok_or_else(|| not_found(format!("stream {child}")))?;
            let parent = match &record.parent {
                Some(p) => reader.stream(p).map_err(meta_error)?,
                None => None,
            };
            if let Some(parent) = parent {
                let nodes = reader.nodes().map_err(meta_error)?;
                for d in Dirs::new(&nodes)
                    .diff_paths(Some(parent.root), record.root)
                    .map_err(meta_error)?
                {
                    access.require(
                        Level::Write,
                        &parent.path.join(&d.path).map_err(super::invalid)?,
                    )?;
                }
            }
        }
        Ok((child, access))
    })
    .await?;
    let proposal = Proposal::CopyUp {
        from: child,
        author: access.name().to_owned(),
        description,
        at: state.clock.now(),
    };
    let (_, cl, root) = state.propose(proposal).await?.submitted();
    Ok(Reply::Submitted { cl: cl.0, root })
}

pub(super) fn file_info(kind: Option<EntryKind>) -> Option<FileInfo> {
    match kind? {
        EntryKind::File {
            content,
            size,
            executable,
        } => Some(FileInfo {
            content,
            size,
            executable,
        }),
        EntryKind::Dir { .. } | EntryKind::Symlink { .. } => None,
    }
}

fn info(s: Stream) -> StreamInfo {
    StreamInfo {
        path: s.path.to_string(),
        kind: proto_kind(s.kind),
        parent: s.parent.map(|p| p.to_string()),
        cl: s.head.0,
        root: s.root,
    }
}

fn meta_kind(kind: kelp_proto::rpc::StreamKind) -> StreamKind {
    use kelp_proto::rpc::StreamKind as P;
    match kind {
        P::Mainline => StreamKind::Mainline,
        P::Development => StreamKind::Development,
        P::Release => StreamKind::Release,
        P::Virtual => StreamKind::Virtual,
        P::Task => StreamKind::Task,
    }
}

fn proto_kind(kind: StreamKind) -> kelp_proto::rpc::StreamKind {
    use kelp_proto::rpc::StreamKind as P;
    match kind {
        StreamKind::Mainline => P::Mainline,
        StreamKind::Development => P::Development,
        StreamKind::Release => P::Release,
        StreamKind::Virtual => P::Virtual,
        StreamKind::Task => P::Task,
    }
}
