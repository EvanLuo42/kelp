use std::collections::BTreeSet;

use kelp_core::{ContentId, DepotPath};

use super::{FileDiff, Workspace, stream_relative};
use crate::fsutil::{remove_file, set_read_only};
use crate::progress::Phase;
use crate::{Connection, ContentCache, Error, Result};

/// What [`Workspace::clean`] did, or would do.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanReport {
    /// Tracked files changed or missing on disk, put back as synced.
    pub restored: Vec<DepotPath>,
    /// Files the workspace does not track, deleted.
    pub removed: Vec<DepotPath>,
}

/// Returns whether `path` is one of `filter` or below one, or `filter` is
/// empty.
fn wanted(filter: &[DepotPath], path: &DepotPath) -> bool {
    filter.is_empty() || filter.iter().any(|f| path.starts_with(f))
}

impl Workspace {
    /// Returns how files on disk differ from the versions last synced: the
    /// opened files and those changed or deleted without being opened,
    /// limited to `filter` unless it is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if a file cannot be read or a synced version cannot
    /// be fetched.
    pub async fn diff(
        &self,
        conn: &Connection,
        cache: &ContentCache,
        filter: &[DepotPath],
    ) -> Result<Vec<FileDiff>> {
        let status = self.status()?;
        let files: BTreeSet<DepotPath> = status
            .opened
            .iter()
            .map(|(p, ..)| p.clone())
            .chain(status.modified)
            .chain(status.deleted)
            .filter(|p| wanted(filter, p))
            .collect();
        let synced: Vec<ContentId> = files
            .iter()
            .filter_map(|p| self.have_version(p).map(|h| h.content))
            .collect();
        cache.fetch(conn, &synced).await?;
        let mut diffs = Vec::new();
        for path in files {
            let old = match self.have_version(&path) {
                Some(have) => Some(cache.read(&have.content).await?),
                None => None,
            };
            let new = match std::fs::read(self.local_path(&path)?) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            if old != new {
                diffs.push(FileDiff { path, old, new });
            }
        }
        Ok(diffs)
    }

    /// Puts files that are not opened back as last synced, and deletes
    /// files the workspace does not track, limited to `filter` unless it is
    /// empty. With `dry_run`, only reports what it would do.
    ///
    /// Opened files are left alone; revert them to discard their changes.
    /// Changes discarded by cleaning are not kept anywhere.
    ///
    /// # Errors
    ///
    /// Returns an error if a file cannot be written or deleted, or a synced
    /// version cannot be fetched.
    pub async fn clean(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        filter: &[DepotPath],
        dry_run: bool,
    ) -> Result<CleanReport> {
        let status = self.refresh_status(None)?;
        let mut report = CleanReport {
            restored: status
                .modified
                .into_iter()
                .chain(status.deleted)
                .filter(|p| wanted(filter, p))
                .collect(),
            removed: status
                .added
                .into_iter()
                .filter(|p| wanted(filter, p))
                .collect(),
        };
        report.restored.sort();
        if dry_run {
            return Ok(report);
        }
        let synced: Vec<ContentId> = report
            .restored
            .iter()
            .filter_map(|p| self.have_version(p).map(|h| h.content))
            .collect();
        cache.fetch(conn, &synced).await?;
        let tmp = self.tmp_dir();
        let mut meter = cache.meter(Phase::Write, report.restored.len() as u64, None);
        for path in &report.restored {
            let have = self
                .have_version(path)
                .ok_or_else(|| Error::Invalid(format!("{path} was never synced")))?;
            meter.add(1, have.size);
            let abs = self.local_path(path)?;
            if abs.exists() {
                set_read_only(&abs, false)?;
            }
            cache
                .materialize(&have.content, have.executable, &abs, &tmp)
                .await?;
            set_read_only(&abs, self.is_exclusive(path))?;
            self.record_have(path, have)?;
        }
        for path in &report.removed {
            remove_file(&self.local_path(path)?, &self.root)?;
        }
        self.save()?;
        Ok(report)
    }
}

/// Returns the content of the file at `path` in its stream as of changelist
/// `at`, or the head.
///
/// # Errors
///
/// Returns [`Error::Invalid`] if no stream contains `path` or the file does
/// not exist then, or an error if the server cannot be reached.
pub async fn read_file(
    conn: &Connection,
    cache: &ContentCache,
    path: &DepotPath,
    at: Option<u64>,
) -> Result<Vec<u8>> {
    let kelp_proto::Reply::Streams(streams) = conn.call(&kelp_proto::Request::ListStreams).await?
    else {
        return Err(Error::UnexpectedReply);
    };
    let stream = streams
        .into_iter()
        .filter_map(|s| DepotPath::parse(&s.path).ok())
        .filter(|s| path.starts_with(s) && s != path)
        .max_by_key(|s| s.as_str().len())
        .ok_or_else(|| Error::Invalid(format!("no stream contains {path}")))?;
    let head = conn.resolve_stream(stream.as_str(), at).await?;
    let rel = stream_relative(&stream, path);
    let (_, lookups) = conn
        .diff_tree(stream.as_str(), Some(head.root), head.root, vec![rel])
        .await?;
    let file = lookups
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| Error::Invalid(format!("{path} does not exist there")))?;
    cache.fetch(conn, &[file.content]).await?;
    cache.read(&file.content).await
}

/// One line of a file with the changelist that last changed it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AnnotatedLine {
    pub cl: u64,
    pub text: String,
}

/// Returns the lines of the text file at `path` as of its latest revision,
/// each with the changelist that last changed it, following the file
/// through moves.
///
/// # Errors
///
/// Returns [`Error::Invalid`] if the file has no revisions, is deleted or
/// is not text, or an error if the server cannot be reached.
pub async fn annotate(
    conn: &Connection,
    cache: &ContentCache,
    path: &DepotPath,
) -> Result<Vec<AnnotatedLine>> {
    let request = kelp_proto::Request::Filelog {
        path: path.to_string(),
    };
    let kelp_proto::Reply::Filelog(mut revisions) = conn.call(&request).await? else {
        return Err(Error::UnexpectedReply);
    };
    revisions.reverse();
    let contents: Vec<ContentId> = revisions.iter().filter_map(|r| r.content).collect();
    cache.fetch(conn, &contents).await?;
    let mut lines: Vec<AnnotatedLine> = Vec::new();
    let mut deleted = true;
    for revision in revisions {
        let Some(content) = revision.content else {
            // A deletion starts the file over; a purged revision keeps
            // what came before.
            if matches!(revision.action, kelp_proto::rpc::FileAction::Delete) {
                lines.clear();
                deleted = true;
            }
            continue;
        };
        let bytes = cache.read(&content).await?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::Invalid(format!("{path} is not a text file")))?;
        let old: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        let old_text: String = old.concat();
        let diff = similar::TextDiff::from_lines(&old_text, &text);
        let mut next = Vec::new();
        for op in diff.ops() {
            for change in diff.iter_changes(op) {
                match change.tag() {
                    similar::ChangeTag::Equal => {
                        let i = change.old_index().expect("an equal line has an old index");
                        next.push(lines[i].clone());
                    }
                    similar::ChangeTag::Insert => next.push(AnnotatedLine {
                        cl: revision.cl,
                        text: change.value().to_owned(),
                    }),
                    similar::ChangeTag::Delete => {}
                }
            }
        }
        lines = next;
        deleted = false;
    }
    if deleted {
        return Err(Error::Invalid(format!("{path} does not exist")));
    }
    Ok(lines)
}
