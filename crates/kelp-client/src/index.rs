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
        let local = ws.relative(abs);
        if local.is_empty() || is_meta_path(&local) {
            return Ok(());
        }
        let prefix = format!("{local}/");
        self.files
            .retain(|l, _| *l != local && !l.starts_with(&prefix));
        match std::fs::symlink_metadata(abs) {
            Ok(meta) if meta.is_file() => {
                if !(Ignore::load(ws.root())?.is_ignored(&local) || ws.in_artifact_dir(&local)) {
                    self.files.insert(local, DiskEntry::of(&meta));
                }
            }
            Ok(meta) if meta.is_dir() => self.files.extend(ws.walk_entries(&local)?),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}
