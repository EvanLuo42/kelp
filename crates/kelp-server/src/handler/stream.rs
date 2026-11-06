use std::collections::HashMap;
use std::sync::Arc;

use kelp_core::{ClNumber, DepotPath, TreeId};
use kelp_meta::acl::Level;
use kelp_meta::db::{Proposal, Reader};
use kelp_meta::record::{Import, Stream};
use kelp_meta::tree::Visible;
use kelp_meta::{Command, Dirs, EntryKind, StreamKind};
use kelp_proto::rpc::{
    DirEntry, DirEntryKind, FileDelta, FileInfo, ImportInfo, IntegrationInfo, LastChange,
    ListOptions, StreamInfo,
};
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::{Access, Response, blocking, meta_error, not_found, parse_path};
use crate::State;

/// How many streams up [`last_change`] looks for a file's revisions.
const MAX_PARENTS: usize = 64;

/// A changelist's author, the author's display name and when it was submitted.
type ChangeAuthor = (String, Option<String>, u64);

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
    let reader = state.reader()?;
    let record = reader
        .stream(&path)
        .map_err(meta_error)?
        .ok_or_else(|| not_found(format!("stream {path}")))?;
    // Administrators of the stream delete any; the owner of a task stream
    // may delete it too. A stream with no recorded owner predates owners.
    let owner = reader.stream_owner(&path).map_err(meta_error)?;
    drop(reader);
    let owns = record.kind == StreamKind::Task
        && owner
            .is_some_and(|o| kelp_meta::db::name_key(&o) == kelp_meta::db::name_key(access.name()));
    if !owns && !access.administers(&path) {
        return Err(ErrorReply::new(
            ErrorKind::Forbidden,
            match record.kind {
                StreamKind::Task => format!("task stream {path} belongs to someone else"),
                _ => format!("deleting {path} needs an administrator of it"),
            },
        ));
    }
    state
        .commit(access.name(), Command::DeleteStream { path })
        .await?;
    Ok(Reply::Ok)
}

/// Creates a stream. Administrators of the stream create any kind; other
/// users who can read every file of the parent may create task streams of it.
pub(super) async fn create_stream(
    state: &Arc<State>,
    access: Access,
    path: String,
    kind: kelp_proto::rpc::StreamKind,
    parent: Option<String>,
) -> Response {
    let kind = meta_kind(kind);
    let parent = parent.as_deref().map(parse_path).transpose()?;
    let path = parse_path(&path)?;
    let access = if access.administers(&path) {
        access
    } else {
        match (&parent, kind) {
            (Some(p), StreamKind::Task) => {
                access.require_stream(p)?;
                let (checking, p) = (state.clone(), p.clone());
                blocking(move || {
                    require_whole_tree(&checking, &access, &p)?;
                    Ok(access)
                })
                .await?
            }
            _ => {
                return Err(ErrorReply::new(
                    ErrorKind::Forbidden,
                    format!("only administrators of {path} create this kind of stream"),
                ));
            }
        }
    };
    let cmd = Command::CreateStream {
        path,
        kind,
        parent,
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

/// Refuses unless `access` may read every file `stream` has now.
///
/// A task stream starts with a copy of its parent's tree under its own
/// path, where protections written for the parent's paths no longer apply,
/// so making one must not hand a user files of the parent they cannot read.
fn require_whole_tree(
    state: &State,
    access: &Access,
    stream: &DepotPath,
) -> Result<(), ErrorReply> {
    let reader = state.reader()?;
    let record = reader.stream(stream).map_err(meta_error)?;
    let record = record.ok_or_else(|| not_found(format!("stream {stream}")))?;
    let real_paths = reader.real_paths().map_err(meta_error)?;
    let nodes = reader.nodes().map_err(meta_error)?;
    for d in Dirs::new(&nodes)
        .diff_paths(None, record.root)
        .map_err(meta_error)?
    {
        let path = stream.join(&d.path).map_err(super::invalid)?;
        let real = real_paths.real_path(&path).map_err(meta_error)?;
        if !access.can(Level::Read, &path) || !access.can(Level::Read, &real) {
            return Err(ErrorReply::new(
                ErrorKind::Forbidden,
                format!(
                    "you cannot read every file of {stream}, so you cannot make a task stream of it; ask an administrator"
                ),
            ));
        }
    }
    Ok(())
}

/// Refuses unless every tree of `trees` is one that `stream` had, on its
/// own or with its imports, as of some changelist: what
/// [`resolve`] and [`integration`] hand out for it.
///
/// Diffs are filtered by the protections of the paths they would have in
/// `stream`, which only holds for trees of `stream`; a tree of another
/// stream would show its files under paths the user may read.
///
/// Meta keeps no index from trees to streams, so this walks the stream's
/// history back from its head until every tree is found. Clients diff
/// against recent trees, so the walk is usually short.
fn require_stream_trees(
    reader: &Reader,
    stream: &DepotPath,
    trees: &[TreeId],
) -> Result<(), ErrorReply> {
    let record = reader.stream(stream).map_err(meta_error)?;
    let record = record.ok_or_else(|| not_found(format!("stream {stream}")))?;
    let mut missing: Vec<TreeId> = trees
        .iter()
        .copied()
        .filter(|t| *t != record.root && *t != record.base_root)
        .collect();
    let mut at = None;
    while !missing.is_empty() {
        let (cl, view) = reader.view_at(stream, at).map_err(meta_error)?;
        let (_, own) = reader.stream_at(stream, at).map_err(meta_error)?;
        missing.retain(|t| *t != view && *t != own);
        match cl.0.checked_sub(1) {
            Some(before) => at = Some(ClNumber(before)),
            None => break,
        }
    }
    match missing.first() {
        None => Ok(()),
        Some(tree) => Err(ErrorReply::new(
            ErrorKind::Invalid,
            format!("tree {tree} is not a revision of {stream}; sync again"),
        )),
    }
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
        let trees: Vec<TreeId> = from.into_iter().chain([to]).collect();
        require_stream_trees(&reader, &stream, &trees)?;
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

pub(super) async fn list_dir(
    state: &Arc<State>,
    access: Access,
    stream: String,
    path: String,
    options: ListOptions,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let state = state.clone();
    blocking(move || {
        let ListOptions {
            at,
            depth,
            dirs_only,
            details,
        } = options;
        let reader = state.reader()?;
        if reader.stream(&stream).map_err(meta_error)?.is_none() {
            return Err(not_found(format!("stream {stream}")));
        }
        let (_, root) = reader
            .view_at(&stream, at.map(ClNumber))
            .map_err(meta_error)?;
        let real_paths = reader.real_paths().map_err(meta_error)?;
        let files = |rel: &str| {
            stream.join(rel).is_ok_and(|p| {
                real_paths
                    .real_path(&p)
                    .is_ok_and(|real| access.can(Level::Read, &p) && access.can(Level::Read, &real))
            })
        };
        let dirs_visible = |rel: &str| stream.join(rel).is_ok_and(|p| access.sees(&p));
        let visible = Visible {
            files: &files,
            dirs: &dirs_visible,
        };
        let nodes = reader.nodes().map_err(meta_error)?;
        let dirs = Dirs::new(&nodes);
        let mut listed = Vec::new();
        if path.is_empty() {
            dirs.list_visible(root, "", depth, &visible, &mut listed)
                .map_err(meta_error)?;
        } else {
            let missing = || not_found(format!("{stream}/{path}"));
            let entry = dirs.lookup(root, &path).map_err(meta_error)?;
            match entry.ok_or_else(missing)? {
                kelp_meta::TreeEntry {
                    kind: EntryKind::Dir { tree },
                    ..
                } => {
                    if !dirs
                        .has_visible(tree, &path, &visible)
                        .map_err(meta_error)?
                    {
                        return Err(missing());
                    }
                    dirs.list_visible(tree, &format!("{path}/"), depth, &visible, &mut listed)
                        .map_err(meta_error)?;
                }
                entry => {
                    if !files(&path) {
                        return Err(missing());
                    }
                    let spelled = match path.rsplit_once('/') {
                        Some((parent, _)) => format!("{parent}/{}", entry.name),
                        None => entry.name,
                    };
                    listed.push((spelled, entry.kind));
                }
            }
        }
        if dirs_only {
            listed.retain(|(_, kind)| matches!(kind, EntryKind::Dir { .. }));
        }
        let mut authors = HashMap::new();
        let mut entries = Vec::with_capacity(listed.len());
        for (path, kind) in listed {
            let (kind, last) = match kind {
                EntryKind::File {
                    content,
                    size,
                    executable,
                } => {
                    let last = match details {
                        true => {
                            let file = stream.join(&path).map_err(super::invalid)?;
                            let parts = (&reader, &access, &real_paths);
                            let at = at.map(ClNumber);
                            last_change(parts, &stream, &file, content, at, &mut authors)?
                        }
                        false => None,
                    };
                    let info = FileInfo {
                        content,
                        size,
                        executable,
                    };
                    (DirEntryKind::File(info), last)
                }
                EntryKind::Dir { .. } => (DirEntryKind::Dir, None),
                EntryKind::Symlink { target } => (DirEntryKind::Symlink { target }, None),
            };
            entries.push(DirEntry { path, kind, last });
        }
        Ok(Reply::DirEntries(entries))
    })
    .await
}

/// Returns the newest revision, as of changelist `at` if given, that gave the
/// file `file` of `stream` its `content`.
///
/// A file a stream inherited has no revisions of its own there, so this looks
/// in the streams it branched from too. It gives up at a path the user may not
/// read, which would show what happened there.
fn last_change(
    (reader, access, real_paths): (&Reader, &Access, &kelp_meta::db::RealPaths),
    stream: &DepotPath,
    file: &DepotPath,
    content: kelp_core::ContentId,
    at: Option<ClNumber>,
    authors: &mut HashMap<u64, Option<ChangeAuthor>>,
) -> Result<Option<LastChange>, ErrorReply> {
    let rel = kelp_meta::db::stream_relative(stream, file).unwrap_or_default();
    let mut current = Some(stream.clone());
    // Parents cannot form a cycle, but a corrupt record should not hang a listing.
    for _ in 0..MAX_PARENTS {
        let Some(owner) = current else { break };
        let path = owner.join(&rel).map_err(super::invalid)?;
        let real = real_paths.real_path(&path).map_err(meta_error)?;
        if !access.can(Level::Read, &path) || !access.can(Level::Read, &real) {
            break;
        }
        let revisions = reader.revisions(&real).map_err(meta_error)?;
        let found = revisions
            .into_iter()
            .find(|r| at.is_none_or(|at| r.cl <= at) && r.content == Some(content));
        if let Some(revision) = found {
            let cl = revision.cl.0;
            let author = match authors.get(&cl) {
                Some(author) => author.clone(),
                None => {
                    let change = reader.changelist(revision.cl).map_err(meta_error)?;
                    let author = match change {
                        Some(c) => {
                            let profile = reader.profile_of(&c.author).map_err(meta_error)?;
                            Some((c.author, profile.display_name, c.submitted_at))
                        }
                        None => None,
                    };
                    authors.insert(cl, author.clone());
                    author
                }
            };
            return Ok(
                author.map(|(author, author_name, submitted_at)| LastChange {
                    rev: revision.rev,
                    cl,
                    author,
                    author_name,
                    submitted_at,
                }),
            );
        }
        current = reader
            .stream(&owner)
            .map_err(meta_error)?
            .and_then(|s| s.parent);
    }
    Ok(None)
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
