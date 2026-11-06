//! An in-memory listing of a workspace's files, kept current by a watcher.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::Path;
use std::time::UNIX_EPOCH;

use kelp_workspace::Ignore;
use kelp_workspace::ignore::is_meta_path;
use serde::{Deserialize, Serialize};

use crate::{Result, Workspace};

/// The size and modification time of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskEntry {
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime: u64,
}

impl DiskEntry {
    /// Returns the entry for `meta`.
    pub fn of(meta: &Metadata) -> Self {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        Self {
            size: meta.len(),
            mtime,
        }
    }
}

/// The non-ignored files of a workspace with their metadata.
///
/// A full [`scan`](Self::scan) lists every directory; afterwards
/// [`refresh`](Self::refresh) updates the entries below one changed path, so
/// a file watcher can keep the index current cheaply.
#[derive(Clone, Debug, Default)]
pub struct DiskIndex {
    files: BTreeMap<String, DiskEntry>,
}

impl DiskIndex {
    /// Lists the workspace's files.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace cannot be read.
    pub fn scan(ws: &Workspace) -> Result<Self> {
        Ok(Self {
            files: ws.walk_entries("")?.into_iter().collect(),
        })
    }

    /// Returns the files in path order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &DiskEntry)> {
        self.files.iter().map(|(l, e)| (l.as_str(), e))
    }

    /// Returns the number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Returns `true` if there are no files.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Updates the entries at and below `abs`, an absolute path that changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the path exists but cannot be read.
    pub fn refresh(&mut self, ws: &Workspace, abs: &Path) -> Result<()> {
        self.refresh_many(ws, [abs])
    }

    /// Updates the entries at and below each of `paths`, as
    /// [`refresh`](Self::refresh) does for one.
    ///
    /// Reads the ignore rules once, skips paths they exclude (the index never
    /// holds any, so a build writing thousands of files into an ignored
    /// directory costs next to nothing), and skips paths below another one in
    /// `paths`, which walking that one covers.
    ///
    /// # Errors
    ///
    /// Returns an error if a path exists but cannot be read.
    pub fn refresh_many<P: AsRef<Path>>(
        &mut self,
        ws: &Workspace,
        paths: impl IntoIterator<Item = P>,
    ) -> Result<()> {
        let mut ignore = None;
        let mut locals = Vec::new();
        for abs in paths {
            let local = ws.relative(abs.as_ref());
            if local.is_empty() || is_meta_path(&local) {
                continue;
            }
            if ignore.is_none() {
                ignore = Some(Ignore::load(ws.root())?);
            }
            let rules = ignore.as_ref().expect("loaded above");
            // A path below an ignored directory matches the rule for the
            // directory's contents even when it is not known to be a directory.
            if rules.is_ignored(&local, false) || ws.in_artifact_dir(&local) {
                continue;
            }
            locals.push(local);
        }
        let Some(ignore) = ignore else {
            return Ok(());
        };
        locals.sort_unstable();
        locals.dedup();
        let mut covered: Option<String> = None;
        for local in locals {
            if covered
                .as_ref()
                .is_some_and(|c| local.starts_with(c.as_str()))
            {
                continue;
            }
            covered = Some(format!("{local}/"));
            self.remove_at_and_below(&local);
            let abs = ws.abs(&local);
            match std::fs::symlink_metadata(&abs) {
                Ok(meta) if meta.is_file() => {
                    self.files.insert(local, DiskEntry::of(&meta));
                }
                Ok(meta) if meta.is_dir() => {
                    self.files.extend(ws.walk_entries_with(&ignore, &local)?);
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Removes the entry for `local` and those below it. The keys below a
    /// directory are contiguous, so this does not look at the other files.
    fn remove_at_and_below(&mut self, local: &str) {
        self.files.remove(local);
        let prefix = format!("{local}/");
        let below: Vec<String> = self
            .files
            .range(prefix.clone()..)
            .map(|(l, _)| l)
            .take_while(|l| l.starts_with(&prefix))
            .cloned()
            .collect();
        for l in below {
            self.files.remove(&l);
        }
    }
}
