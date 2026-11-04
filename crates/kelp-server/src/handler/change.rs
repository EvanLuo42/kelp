use std::sync::Arc;

use kelp_core::{ClNumber, DepotPath};
use kelp_meta::acl::Level;
use kelp_meta::db::{Outcome, Proposal};
use kelp_meta::record::{Changelist, RevAction};
use kelp_meta::{
    Command, FileAction, FileChange, FileData, Integration, LockDomain, LockHolder, ShelfContents,
    Submission,
};
use kelp_proto::rpc::{
    ChangeDetail, ChangeSummary, FileInfo, LockInfo, RevisionInfo, ShelfInfo, SubmitAction,
    SubmitChange,
};
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::{Access, Response, internal, invalid, meta_error, not_found, parse_path};
use crate::State;

/// The fields of [`Request::Submit`](kelp_proto::Request::Submit).
pub(super) struct SubmitArgs {
    pub stream: String,
    pub workspace: String,
    pub description: String,
    pub changes: Vec<SubmitChange>,
    pub integration: Option<(String, u64)>,
    pub review: Option<u64>,
    pub fixes: Vec<String>,
    pub moves: Vec<(String, String)>,
}

pub(super) async fn submit(state: &Arc<State>, access: Access, args: SubmitArgs) -> Response {
    let SubmitArgs {
        stream,
        workspace,
        description,
        changes,
        integration,
        review,
        fixes,
        moves,
    } = args;
    let moves = moves
        .iter()
        .map(|(from, to)| Ok((parse_path(from)?, parse_path(to)?)))
        .collect::<Result<Vec<_>, ErrorReply>>()?;
    let fixes = fixes
        .iter()
        .map(|k| {
            kelp_core::issue::parse_key(k)
                .ok_or_else(|| invalid(format!("{k:?} is not an issue key like PROJ-123")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if workspace.is_empty() {
        return Err(ErrorReply::new(
            ErrorKind::Invalid,
            "submits need a workspace name",
        ));
    }
    let stream = parse_path(&stream)?;
    let changes = changes
        .into_iter()
        .map(|c| to_meta(&stream, c))
        .collect::<Result<Vec<_>, _>>()?;
    {
        let real_paths = state.reader()?.real_paths().map_err(meta_error)?;
        for change in &changes {
            access.require(Level::Write, &change.path)?;
            access.require(
                Level::Write,
                &real_paths.real_path(&change.path).map_err(meta_error)?,
            )?;
        }
    }
    let integration = match integration {
        Some((source, cl)) => Some(Integration {
            source: parse_path(&source)?,
            through: ClNumber(cl),
        }),
        None => None,
    };
    let typemap = state
        .reader()?
        .typemap(stream.depot())
        .map_err(meta_error)?;
    let typemap = typemap
        .map(|r| kelp_core::typemap::Typemap::parse(&r.source))
        .transpose()
        .map_err(internal)?;
    let mut issues = kelp_core::issue::find_keys(
        &description,
        typemap.as_ref().and_then(|t| t.issue_prefixes()),
    );
    issues.extend(
        fixes
            .iter()
            .filter(|k| !issues.contains(k))
            .cloned()
            .collect::<Vec<_>>(),
    );
    if let Some(typemap) = &typemap {
        crate::plugins::check_triggers(
            state,
            typemap,
            &stream,
            access.name(),
            &description,
            &changes,
        )
        .await?;
    }
    let submission = Submission {
        stream,
        author: access.name().to_owned(),
        workspace,
        description,
        changes,
        integration,
        issues,
        fixes,
        review: review.map(ClNumber),
        at: state.clock.now(),
    };
    let proposal = match moves.is_empty() {
        true => Proposal::Submit(submission),
        false => Proposal::SubmitMoves { submission, moves },
    };
    let (_, cl, root) = state.propose(proposal).await?.submitted();
    Ok(Reply::Submitted { cl: cl.0, root })
}

pub(super) fn filelog(state: &State, access: &Access, path: String) -> Response {
    let path = parse_path(&path)?;
    access.require(Level::Read, &path)?;
    let reader = state.reader()?;
    let real = reader.real_path(&path).map_err(meta_error)?;
    access.require(Level::Read, &real)?;
    let revisions = reader.history(&real).map_err(meta_error)?;
    let infos = revisions
        .into_iter()
        .filter(|r| access.can(Level::Read, &r.path))
        .map(|r| RevisionInfo {
            path: r.path.to_string(),
            rev: r.rev,
            cl: r.cl.0,
            action: rev_action(r.action),
            content: r.content,
            size: r.size,
            purged: r.purged,
        })
        .collect();
    Ok(Reply::Filelog(infos))
}

pub(super) fn changes(
    state: &State,
    access: &Access,
    stream: Option<String>,
    limit: u32,
) -> Response {
    let stream = stream.as_deref().map(parse_path).transpose()?;
    if let Some(s) = &stream {
        access.require_stream(s)?;
    }
    let reader = state.reader()?;
    let changes = reader
        .changes(stream.as_ref(), limit.min(1000) as usize)
        .map_err(meta_error)?;
    Ok(Reply::Changes(
        changes
            .iter()
            .filter(|c| access.sees(&c.stream))
            .map(|c| summary(&reader, c))
            .collect::<Result<_, _>>()?,
    ))
}

pub(super) fn describe(state: &State, access: &Access, cl: u64) -> Response {
    let not_found = || not_found(format!("changelist {cl}"));
    let reader = state.reader()?;
    let record = reader
        .changelist(ClNumber(cl))
        .map_err(meta_error)?
        .ok_or_else(not_found)?;
    if !access.sees(&record.stream) {
        return Err(not_found());
    }
    let files = record
        .changes
        .iter()
        .filter(|c| access.can(Level::Read, &c.path))
        .map(|c| (c.path.to_string(), file_action(&c.action)))
        .collect();
    Ok(Reply::Describe(ChangeDetail {
        summary: summary(&reader, &record)?,
        files,
    }))
}

/// Locks stream-relative `paths`, all or none. Paths below an import are
/// locked in the import's source stream, so the lock is seen by everyone
/// editing that file.
pub(super) async fn acquire_locks(
    state: &State,
    access: &Access,
    stream: String,
    workspace: String,
    paths: Vec<String>,
) -> Response {
    let targets = lock_targets(state, access, &stream, paths)?;
    let holder = LockHolder {
        user: access.name().to_owned(),
        workspace,
    };
    let commands = targets
        .into_iter()
        .map(|(domain, paths)| Command::AcquireLocks {
            holder: holder.clone(),
            domain,
            paths,
        })
        .collect();
    state.commit_all(access.name(), commands).await?;
    Ok(Reply::Ok)
}

pub(super) async fn release_locks(
    state: &State,
    access: &Access,
    stream: String,
    workspace: String,
    paths: Vec<String>,
    force: bool,
) -> Response {
    let holder = match force {
        true => {
            access.require_administers(&parse_path(&stream)?)?;
            None
        }
        false => Some(LockHolder {
            user: access.name().to_owned(),
            workspace,
        }),
    };
    let commands = lock_targets(state, access, &stream, paths)?
        .into_iter()
        .map(|(domain, paths)| Command::ReleaseLocks {
            holder: holder.clone(),
            domain,
            paths,
        })
        .collect();
    state.commit_all(access.name(), commands).await?;
    Ok(Reply::Ok)
}

pub(super) fn list_locks(state: &State, access: &Access, stream: String) -> Response {
    let (_, domain) = lock_domain(state, access, &stream)?;
    let locks = state.reader()?.locks(&domain).map_err(meta_error)?;
    let infos = locks
        .into_iter()
        .map(|l| LockInfo {
            path: l.path,
            user: l.holder.user,
            workspace: l.holder.workspace,
        })
        .collect();
    Ok(Reply::Locks(infos))
}

pub(super) async fn shelve(
    state: &State,
    access: &Access,
    shelf: Option<u64>,
    stream: String,
    workspace: String,
    description: String,
    files: Vec<SubmitChange>,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let files = files
        .into_iter()
        .map(|c| to_meta(&stream, c))
        .collect::<Result<Vec<_>, _>>()?;
    {
        let real_paths = state.reader()?.real_paths().map_err(meta_error)?;
        for file in &files {
            access.require(Level::Write, &file.path)?;
            access.require(
                Level::Write,
                &real_paths.real_path(&file.path).map_err(meta_error)?,
            )?;
        }
    }
    if let Some(cl) = shelf {
        let existing = state.reader()?.shelf(ClNumber(cl)).map_err(meta_error)?;
        let existing = existing.ok_or_else(|| not_found(format!("shelf {cl}")))?;
        if existing.contents.owner != access.name() {
            return Err(ErrorReply::new(
                ErrorKind::Forbidden,
                format!("shelf {cl} belongs to someone else"),
            ));
        }
    }
    let contents = ShelfContents {
        stream,
        owner: access.name().to_owned(),
        workspace,
        description,
        files,
        at: state.clock.now(),
    };
    let proposal = Proposal::Shelve {
        replace: shelf.map(ClNumber),
        contents,
    };
    match state.propose(proposal).await? {
        Outcome::Shelved { cl } => Ok(Reply::Shelved { cl: cl.0 }),
        _ => Err(internal("shelving produced no shelf")),
    }
}

pub(super) fn get_shelf(state: &State, access: &Access, cl: u64) -> Response {
    let not_found = || not_found(format!("shelf {cl}"));
    let shelf = state
        .reader()?
        .shelf(ClNumber(cl))
        .map_err(meta_error)?
        .ok_or_else(not_found)?;
    let c = shelf.contents;
    if !access.sees(&c.stream) {
        return Err(not_found());
    }
    let files = c
        .files
        .iter()
        .filter(|f| access.can(Level::Read, &f.path))
        .map(|f| to_proto(&c.stream, f))
        .collect();
    Ok(Reply::Shelf(ShelfInfo {
        cl,
        stream: c.stream.to_string(),
        owner: c.owner,
        workspace: c.workspace,
        description: c.description,
        files,
    }))
}

pub(super) async fn delete_shelf(state: &State, access: &Access, cl: u64) -> Response {
    let not_found = || not_found(format!("shelf {cl}"));
    let shelf = state
        .reader()?
        .shelf(ClNumber(cl))
        .map_err(meta_error)?
        .ok_or_else(not_found)?;
    if shelf.contents.owner != access.name() && !access.administers(&shelf.contents.stream) {
        return Err(ErrorReply::new(
            ErrorKind::Forbidden,
            format!("shelf {cl} belongs to someone else"),
        ));
    }
    state
        .commit(access.name(), Command::DeleteShelf { cl: ClNumber(cl) })
        .await?;
    Ok(Reply::Ok)
}

/// Groups stream-relative `paths` by the lock domain they are locked in,
/// with each path relative to the stream owning it, after checking that the
/// user may write them.
fn lock_targets(
    state: &State,
    access: &Access,
    stream: &str,
    paths: Vec<String>,
) -> Result<Vec<(LockDomain, Vec<String>)>, ErrorReply> {
    let (stream, domain) = lock_domain(state, access, stream)?;
    let reader = state.reader()?;
    let mut targets: Vec<(LockDomain, Vec<String>)> = vec![(domain, Vec::new())];
    for path in paths {
        let full = stream.join(&path).map_err(invalid)?;
        access.require(Level::Write, &full)?;
        let Some((import, real)) = reader.route(&stream, &full).map_err(meta_error)? else {
            targets[0].1.push(path);
            continue;
        };
        access.require(Level::Write, &real)?;
        let source = reader.stream(&import.source).map_err(meta_error)?;
        let source = source.ok_or_else(|| {
            ErrorReply::new(
                ErrorKind::NotFound,
                format!("stream {} not found", import.source),
            )
        })?;
        let rel = kelp_meta::db::stream_relative(&source.path, &real)
            .ok_or_else(|| invalid(format!("{path} is the root of an import")))?;
        match targets.iter_mut().find(|(d, _)| *d == source.lock_domain) {
            Some((_, list)) => list.push(rel),
            None => targets.push((source.lock_domain, vec![rel])),
        }
    }
    targets.retain(|(_, paths)| !paths.is_empty());
    Ok(targets)
}

fn lock_domain(
    state: &State,
    access: &Access,
    stream: &str,
) -> Result<(DepotPath, LockDomain), ErrorReply> {
    let stream = parse_path(stream)?;
    access.require_stream(&stream)?;
    let record = state.reader()?.stream(&stream).map_err(meta_error)?;
    let record = record.ok_or_else(|| not_found(format!("stream {stream}")))?;
    Ok((stream, record.lock_domain))
}

fn to_meta(stream: &DepotPath, change: SubmitChange) -> Result<FileChange, ErrorReply> {
    let path = stream.join(&change.path).map_err(invalid)?;
    let data = |f: FileInfo| FileData {
        content: f.content,
        size: f.size,
        executable: f.executable,
    };
    let action = match change.action {
        SubmitAction::Add { file } => FileAction::Add { data: data(file) },
        SubmitAction::Edit { base, file } => FileAction::Edit {
            base,
            data: data(file),
        },
        SubmitAction::Delete { base } => FileAction::Delete { base },
    };
    Ok(FileChange { path, action })
}

pub(super) fn to_proto(stream: &DepotPath, change: &FileChange) -> SubmitChange {
    let file = |d: FileData| FileInfo {
        content: d.content,
        size: d.size,
        executable: d.executable,
    };
    let action = match &change.action {
        FileAction::Add { data } => SubmitAction::Add { file: file(*data) },
        FileAction::Edit { base, data } => SubmitAction::Edit {
            base: *base,
            file: file(*data),
        },
        FileAction::Delete { base } => SubmitAction::Delete { base: *base },
    };
    let path = kelp_meta::db::stream_relative(stream, &change.path).unwrap_or_default();
    SubmitChange { path, action }
}

fn file_action(action: &FileAction) -> kelp_proto::rpc::FileAction {
    match action {
        FileAction::Add { .. } => kelp_proto::rpc::FileAction::Add,
        FileAction::Edit { .. } => kelp_proto::rpc::FileAction::Edit,
        FileAction::Delete { .. } => kelp_proto::rpc::FileAction::Delete,
    }
}

fn rev_action(action: RevAction) -> kelp_proto::rpc::FileAction {
    match action {
        RevAction::Add => kelp_proto::rpc::FileAction::Add,
        RevAction::Edit => kelp_proto::rpc::FileAction::Edit,
        RevAction::Delete => kelp_proto::rpc::FileAction::Delete,
    }
}

pub(super) fn summary(
    reader: &kelp_meta::db::Reader,
    c: &Changelist,
) -> Result<ChangeSummary, ErrorReply> {
    let profile = reader.profile_of(&c.author).map_err(meta_error)?;
    Ok(ChangeSummary {
        cl: c.number.0,
        stream: c.stream.to_string(),
        author: c.author.clone(),
        author_name: profile.display_name,
        author_email: profile.emails.into_iter().next(),
        description: c.description.clone(),
        submitted_at: c.submitted_at,
        issues: c.issues.clone(),
        fixes: c.fixes.clone(),
    })
}
