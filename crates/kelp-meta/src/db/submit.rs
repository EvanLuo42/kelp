use std::collections::HashSet;

use kelp_core::typemap::{LockPolicy, Typemap};
use kelp_core::{ClNumber, ContentId, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use super::apply::Origin;
use super::nodes::TxnNodes;
use super::{
    Applied, CHANGES, LOCKS, MANIFESTS, META, NEXT_CL, REVISIONS, STREAM_CHANGES, STREAMS,
    TYPEMAPS, dec, enc, integration, load_stream, name_key, set_integration, stream_relative,
};
use crate::record::{Changelist, RevAction, Revision, Stream, TypemapRecord};
use crate::tree::fold;
use crate::{
    Dirs, EntryKind, Error, FileAction, FileChange, FileData, Lock, LockHolder, Result, Submission,
    Timestamp,
};

pub(super) fn apply_submit(
    txn: &WriteTransaction,
    cl: ClNumber,
    submission: &Submission,
    origin: Origin,
) -> Result<Applied> {
    let next_cl = txn.open_table(META)?.get(NEXT_CL)?.map_or(1, |v| v.value());
    if cl.0 != next_cl {
        return Err(Error::Invalid(format!(
            "changelist {cl} is not the next number {next_cl}"
        )));
    }
    let changes = &submission.changes;
    let mut stream = load_stream(txn, &submission.stream)?;
    if let Some(integration) = &submission.integration {
        check_integration(txn, &stream, integration)?;
    }
    if changes.is_empty() {
        match &submission.integration {
            // Recording that a stream already has its parent's changes.
            Some(integration) => check_merged(txn, &stream, integration)?,
            None => return Err(Error::Invalid("changelist has no files".into())),
        }
    }
    let holder = LockHolder {
        user: submission.author.clone(),
        workspace: submission.workspace.clone(),
    };
    let server_side = submission.server_side();

    // Changes below a writable import belong to its source stream.
    let imports = super::imports::load(txn, &stream.path)?;
    let mut own = Vec::new();
    let mut routed: Vec<(DepotPath, Vec<FileChange>)> = Vec::new();
    for change in changes {
        let rel = stream_relative(&stream.path, &change.path).ok_or_else(|| {
            Error::Invalid(format!("{} is not in stream {}", change.path, stream.path))
        })?;
        match super::imports::route(&imports, &rel) {
            None => own.push(change.clone()),
            Some((import, _)) if !import.writable => {
                return Err(Error::Invalid(format!(
                    "{} is imported read-only from {}",
                    change.path, import.source
                )));
            }
            Some((import, source_rel)) => {
                let real = FileChange {
                    path: import.source.join(&source_rel)?,
                    action: change.action.clone(),
                };
                match routed.iter_mut().find(|(s, _)| *s == import.source) {
                    Some((_, list)) => list.push(real),
                    None => routed.push((import.source.clone(), vec![real])),
                }
            }
        }
    }

    let mut checked = Vec::new();
    for (target, list) in &routed {
        let target = load_stream(txn, target)?;
        checked.push((
            target.clone(),
            check_changes(txn, &target, list, &holder, server_side, origin)?,
        ));
    }
    let own_checked = check_changes(txn, &stream, &own, &holder, server_side, origin)?;
    super::governance::reviews::gate(txn, cl, submission)?;
    let mut touched = vec![stream.path.clone()];
    for ((mut target, checks), (_, list)) in checked.into_iter().zip(&routed) {
        let root = apply_changes(txn, cl, &target, list, checks)?;
        target.head = cl;
        target.root = root;
        txn.open_table(STREAMS)?
            .insert(target.path.key(), enc(&target).as_slice())?;
        txn.open_table(STREAM_CHANGES)?
            .insert((target.path.key(), cl.0), ())?;
        txn.open_table(super::STREAM_ROOTS)?
            .insert((target.path.key(), cl.0), root.0.0.as_slice())?;
        touched.push(target.path);
    }
    let root = apply_changes(txn, cl, &stream, &own, own_checked)?;

    if let Some(integration) = &submission.integration {
        set_integration(txn, &stream.path, integration)?;
        let copied_up = stream.parent.as_ref() != Some(&integration.source);
        if copied_up {
            // The child now matches its parent, so it has nothing left to merge down.
            let merged = crate::Integration {
                source: stream.path.clone(),
                through: cl,
            };
            set_integration(txn, &integration.source, &merged)?;
        }
    }
    let (issues, fixes) =
        super::governance::issues::canonical_links(&submission.issues, &submission.fixes)?;
    let record = Changelist {
        number: cl,
        stream: stream.path.clone(),
        author: submission.author.clone(),
        description: submission.description.clone(),
        submitted_at: submission.at,
        root,
        changes: changes.clone(),
        integration: submission.integration.clone(),
        issues,
        fixes,
    };
    txn.open_table(CHANGES)?
        .insert(cl.0, enc(&record).as_slice())?;
    txn.open_table(STREAM_CHANGES)?
        .insert((stream.path.key(), cl.0), ())?;
    super::governance::issues::link(txn, cl, &record.issues)?;
    stream.head = cl;
    stream.root = root;
    txn.open_table(STREAMS)?
        .insert(stream.path.key(), enc(&stream).as_slice())?;
    txn.open_table(META)?.insert(NEXT_CL, cl.0 + 1)?;
    super::imports::refresh_views(txn, cl, &touched)?;
    Ok(Applied::Submitted { cl, root })
}

/// What [`check_changes`] found: the tree edits to make and the exclusive
/// locks the submit releases.
struct Checked {
    tree_changes: Vec<(String, Option<EntryKind>)>,
    locked: Vec<String>,
}

/// Checks `changes`, whose paths are in `stream`, against the stream's head,
/// typemap and locks, and that their content was uploaded.
fn check_changes(
    txn: &WriteTransaction,
    stream: &Stream,
    changes: &[FileChange],
    holder: &LockHolder,
    server_side: bool,
    origin: Origin,
) -> Result<Checked> {
    let typemap = load_typemap(txn, stream.path.depot())?;
    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    let (mut conflicts, mut missing, mut tree_changes, mut locked) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let manifests = txn.open_table(MANIFESTS)?;
    let locks = txn.open_table(LOCKS)?;
    let mut seen = HashSet::new();
    for change in changes {
        let path = &change.path;
        let rel = stream_relative(&stream.path, path)
            .ok_or_else(|| Error::Invalid(format!("{path} is not in stream {}", stream.path)))?;
        if !seen.insert(path.key().to_owned()) {
            return Err(Error::Invalid(format!("{path} appears twice")));
        }
        let head = match dirs.lookup(stream.root, &rel)? {
            None => None,
            Some(e) => match e.kind {
                EntryKind::File { content, .. } => Some(content),
                // A directory emptied by the same change can become a
                // file; updating the tree checks that it is.
                EntryKind::Dir { .. } if matches!(change.action, FileAction::Add { .. }) => None,
                _ => {
                    conflicts.push(format!("{path} is a directory"));
                    continue;
                }
            },
        };
        match &change.action {
            FileAction::Add { .. } if head.is_some() => {
                conflicts.push(format!("{path} already exists"))
            }
            FileAction::Edit { base, .. } | FileAction::Delete { base } if head != Some(*base) => {
                conflicts.push(stale(path, head));
            }
            _ => {}
        }
        if typemap
            .as_ref()
            .is_some_and(|t| t.resolve(&rel).lock == LockPolicy::Exclusive)
        {
            let lock = locks.get((stream.lock_domain.0.as_str(), fold(&rel).as_str()))?;
            let lock: Option<Lock> = lock.map(|v| dec("locks", v.value())).transpose()?;
            match lock {
                Some(l) if server_side => conflicts.push(format!(
                    "{path} is locked by {}@{}",
                    l.holder.user, l.holder.workspace
                )),
                Some(l) if l.holder == *holder => locked.push(rel.clone()),
                None if server_side => {}
                _ => conflicts.push(format!(
                    "{path} is exclusive and must be locked by this workspace"
                )),
            }
        }
        let data = change.action.data();
        if let Some(d) = data
            && origin == Origin::Local
            && manifests.get(d.content.0.0.as_slice())?.is_none()
        {
            missing.push(path.to_string());
        }
        tree_changes.push((rel, data.map(file_kind)));
    }
    if !conflicts.is_empty() {
        return Err(Error::Conflict(conflicts));
    }
    if !missing.is_empty() {
        return Err(Error::MissingContent(missing));
    }
    Ok(Checked {
        tree_changes,
        locked,
    })
}

/// Applies checked `changes`, whose paths are in `stream`, to the stream's
/// tree, recording their revisions and releasing their locks, and returns
/// the new tree. The stream record itself is left to the caller.
fn apply_changes(
    txn: &WriteTransaction,
    cl: ClNumber,
    stream: &Stream,
    changes: &[FileChange],
    checked: Checked,
) -> Result<kelp_core::TreeId> {
    let root = {
        let nodes = TxnNodes::new(txn)?;
        Dirs::new(&nodes).update_paths(stream.root, checked.tree_changes)?
    };
    record_revisions(txn, cl, changes)?;
    if let Some(typemap) = load_typemap(txn, stream.path.depot())? {
        for change in changes {
            let rel = stream_relative(&stream.path, &change.path).expect("checked");
            if let Some(keep) = typemap.resolve(&rel).retain {
                purge_old_revisions(txn, &change.path, keep)?;
            }
        }
    }
    let mut locks = txn.open_table(LOCKS)?;
    for rel in checked.locked {
        locks.remove((stream.lock_domain.0.as_str(), fold(&rel).as_str()))?;
    }
    Ok(root)
}

/// Builds the submission that copies `from` into its parent.
pub(super) fn copy_up_submission(
    txn: &WriteTransaction,
    from: &DepotPath,
    author: String,
    description: String,
    at: Timestamp,
) -> Result<Submission> {
    let child = load_stream(txn, from)?;
    if !child.kind.copies_up() {
        return Err(Error::Invalid(format!(
            "{:?} stream {from} cannot copy up",
            child.kind
        )));
    }
    let parent_path = child
        .parent
        .clone()
        .expect("streams that copy up have a parent");
    let parent = load_stream(txn, &parent_path)?;
    let merged = integration(txn, &child.path, &parent.path)?.unwrap_or(ClNumber(0));
    if merged < parent.head {
        return Err(Error::Conflict(vec![format!(
            "{from} has not merged down {parent_path}@{}; run `kelp merge` in a workspace of {from} first",
            parent.head
        )]));
    }
    let nodes = TxnNodes::new(txn)?;
    let changes: Vec<FileChange> = Dirs::new(&nodes)
        .diff_paths(Some(parent.root), child.root)?
        .into_iter()
        .map(|d| {
            let content = |k: Option<EntryKind>| match k {
                Some(EntryKind::File {
                    content,
                    size,
                    executable,
                }) => Some(FileData {
                    content,
                    size,
                    executable,
                }),
                _ => None,
            };
            let action = match (content(d.old), content(d.new)) {
                (None, Some(data)) => FileAction::Add { data },
                (Some(old), Some(data)) => FileAction::Edit {
                    base: old.content,
                    data,
                },
                (Some(old), None) => FileAction::Delete { base: old.content },
                (None, None) => unreachable!("a diff has at least one side"),
            };
            Ok(FileChange {
                path: parent.path.join(&d.path)?,
                action,
            })
        })
        .collect::<Result<_>>()?;
    if changes.is_empty() {
        return Err(Error::Invalid(format!(
            "{from} has nothing to copy to {parent_path}"
        )));
    }
    Ok(Submission {
        stream: parent.path,
        author,
        workspace: String::new(),
        description,
        changes,
        integration: Some(crate::Integration {
            source: child.path,
            through: child.head,
        }),
        issues: Vec::new(),
        fixes: Vec::new(),
        review: None,
        at,
    })
}

fn check_integration(
    txn: &WriteTransaction,
    target: &Stream,
    integration: &crate::Integration,
) -> Result<()> {
    let source = load_stream(txn, &integration.source)?;
    let merge_down = target.parent.as_ref() == Some(&source.path);
    let copy_up = source.parent.as_ref() == Some(&target.path) && source.kind.copies_up();
    if !merge_down && !copy_up {
        return Err(Error::Invalid(format!(
            "{} cannot integrate from {}",
            target.path, source.path
        )));
    }
    if integration.through > source.head {
        return Err(Error::Invalid(format!(
            "{} has no changelist {}",
            source.path, integration.through
        )));
    }
    let current = integration_of(txn, &target.path, &source.path)?;
    if integration.through < current {
        return Err(Error::Conflict(vec![format!(
            "{} already integrated {}@{current}",
            target.path, source.path
        )]));
    }
    Ok(())
}

/// Checks that `target` already has every change its parent made between
/// the last merge-down and `integration.through`, so an integration that
/// changes no file may record the new merge point.
fn check_merged(
    txn: &WriteTransaction,
    target: &Stream,
    integration: &crate::Integration,
) -> Result<()> {
    if target.parent.as_ref() != Some(&integration.source) {
        return Err(Error::Invalid(
            "only a merge-down can be submitted without files".into(),
        ));
    }
    let base = match integration_of(txn, &target.path, &integration.source)? {
        ClNumber(0) => target.base_root,
        merged => super::imports::root_at(txn, &integration.source, merged)?,
    };
    let through = super::imports::root_at(txn, &integration.source, integration.through)?;
    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    let mut missing = Vec::new();
    for diff in dirs.diff_paths(Some(base), through)? {
        let have = dirs.lookup(target.root, &diff.path)?.map(|e| e.kind);
        if have != diff.new {
            missing.push(format!(
                "{} lacks the change to {} from {}",
                target.path, diff.path, integration.source
            ));
        }
    }
    match missing.is_empty() {
        true => Ok(()),
        false => Err(Error::Conflict(missing)),
    }
}

fn integration_of(
    txn: &WriteTransaction,
    target: &DepotPath,
    source: &DepotPath,
) -> Result<ClNumber> {
    Ok(integration(txn, target, source)?.unwrap_or(ClNumber(0)))
}

fn record_revisions(txn: &WriteTransaction, cl: ClNumber, changes: &[FileChange]) -> Result<()> {
    let mut revisions = txn.open_table(REVISIONS)?;
    for change in changes {
        let key = change.path.key();
        let last = revisions
            .range((key, 0u32)..=(key, u32::MAX))?
            .next_back()
            .transpose()?
            .map_or(0, |(k, _)| k.value().1);
        let (action, content, size) = match &change.action {
            FileAction::Add { data } => (RevAction::Add, Some(data.content), data.size),
            FileAction::Edit { data, .. } => (RevAction::Edit, Some(data.content), data.size),
            FileAction::Delete { .. } => (RevAction::Delete, None, 0),
        };
        let rev = Revision {
            path: change.path.clone(),
            rev: last + 1,
            cl,
            action,
            content,
            size,
            purged: false,
        };
        revisions.insert((key, last + 1), enc(&rev).as_slice())?;
    }
    Ok(())
}

/// Marks all but the newest `keep` revisions with content of `path` as purged.
fn purge_old_revisions(txn: &WriteTransaction, path: &DepotPath, keep: u32) -> Result<()> {
    let mut revisions = txn.open_table(REVISIONS)?;
    let key = path.key();
    let mut stale = Vec::new();
    let mut kept = 0;
    for entry in revisions.range((key, 0u32)..=(key, u32::MAX))?.rev() {
        let (k, v) = entry?;
        let rev: Revision = dec("revisions", v.value())?;
        if rev.content.is_none() || rev.purged {
            continue;
        }
        if kept < keep {
            kept += 1;
        } else {
            stale.push((k.value().1, rev));
        }
    }
    for (number, mut rev) in stale {
        rev.purged = true;
        revisions.insert((key, number), enc(&rev).as_slice())?;
    }
    Ok(())
}

fn load_typemap(txn: &WriteTransaction, depot: &str) -> Result<Option<Typemap>> {
    let typemaps = txn.open_table(TYPEMAPS)?;
    let Some(v) = typemaps.get(name_key(depot).as_str())? else {
        return Ok(None);
    };
    let record: TypemapRecord = dec("typemaps", v.value())?;
    Ok(Some(
        Typemap::parse(&record.source).map_err(|e| Error::Invalid(e.to_string()))?,
    ))
}

fn file_kind(d: FileData) -> EntryKind {
    EntryKind::File {
        content: d.content,
        size: d.size,
        executable: d.executable,
    }
}

fn stale(path: &DepotPath, head: Option<ContentId>) -> String {
    match head {
        Some(_) => format!("{path} was changed by someone else; sync and resolve"),
        None => format!("{path} was deleted by someone else"),
    }
}
