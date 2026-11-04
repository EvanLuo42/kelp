use std::collections::BTreeMap;
use std::fs;

use kelp_core::{DepotPath, LocalClId};
use kelp_workspace::{FileVersion, OpenAction, Snapshot, SnapshotFile};

use super::Workspace;
use crate::fsutil::{is_executable, remove_file, set_read_only};
use crate::progress::Phase;
use crate::{Connection, ContentCache, Error, Result};

/// One side of a snapshot comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SnapshotSide {
    /// The snapshot with this id.
    Snapshot(u32),
    /// The files on disk now.
    Working,
}

/// The content of one file on both sides of a comparison.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileDiff {
    pub path: DepotPath,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

impl Workspace {
    /// Saves the opened files of changelist `cl` as a snapshot and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the changelist has no opened files, or an
    /// error if a file cannot be read or cached.
    pub async fn snapshot(
        &mut self,
        cache: &ContentCache,
        cl: LocalClId,
        message: &str,
    ) -> Result<u32> {
        if self.state.pending.changelist(cl).is_none() {
            return Err(kelp_workspace::Error::NoSuchChangelist(cl).into());
        }
        let paths: Vec<DepotPath> = self
            .state
            .pending
            .opened()
            .filter(|(_, _, owner)| *owner == cl)
            .map(|(p, _, _)| p.clone())
            .collect();
        if paths.is_empty() {
            return Err(Error::Invalid(format!(
                "changelist {cl} has no opened files"
            )));
        }
        let id = self
            .capture(cache, &paths, message, false)
            .await?
            .expect("paths are opened");
        self.save()?;
        Ok(id)
    }

    /// Takes automatic snapshots of the opened files among `paths`, one per
    /// changelist, before an operation overwrites them.
    ///
    /// Files opened for delete are included: one may have been created again
    /// on disk, and the snapshot records an absent one as such.
    pub(super) async fn auto_snapshot(
        &mut self,
        cache: &ContentCache,
        paths: &[DepotPath],
        why: &str,
    ) -> Result<()> {
        let opened: Vec<DepotPath> = paths
            .iter()
            .filter(|p| self.state.pending.action(p).is_some())
            .cloned()
            .collect();
        self.capture(cache, &opened, &format!("before {why}"), true)
            .await?;
        // The snapshot must be on disk before the files it protects change.
        self.save()
    }

    async fn capture(
        &mut self,
        cache: &ContentCache,
        paths: &[DepotPath],
        message: &str,
        automatic: bool,
    ) -> Result<Option<u32>> {
        let mut by_cl: BTreeMap<LocalClId, Vec<SnapshotFile>> = BTreeMap::new();
        let mut meter = cache.meter(Phase::Read, paths.len() as u64, None);
        for path in paths {
            meter.add(1, 0);
            let (Some(action), Some(cl)) = (
                self.state.pending.action(path),
                self.state.pending.owner(path),
            ) else {
                continue;
            };
            let abs = self.local_path(path)?;
            let content = match fs::read(&abs) {
                Ok(data) => {
                    let manifest = cache.ingest_durable(&data).await?;
                    Some(FileVersion {
                        content: manifest.content,
                        size: manifest.size,
                        executable: is_executable(&abs)?,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            by_cl.entry(cl).or_default().push(SnapshotFile {
                path: path.clone(),
                action,
                content,
            });
        }
        let mut last = None;
        for (cl, files) in by_cl {
            let id = self.state.snapshots.last().map_or(1, |s| s.id + 1);
            let at = self.clock.now();
            let moves: BTreeMap<DepotPath, DepotPath> = files
                .iter()
                .filter(|f| f.action == OpenAction::MoveTo)
                .filter_map(|f| {
                    let from = self.state.moves.get(&f.path)?;
                    Some((f.path.clone(), from.clone()))
                })
                .collect();
            if moves.is_empty() {
                self.state.snapshot_moves.remove(&id);
            } else {
                self.state.snapshot_moves.insert(id, moves);
            }
            self.state.snapshots.push(Snapshot {
                id,
                cl,
                message: message.to_owned(),
                at,
                automatic,
                files,
            });
            last = Some(id);
        }
        Ok(last)
    }

    /// Returns the snapshot `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if there is no such snapshot.
    pub fn snapshot_by_id(&self, id: u32) -> Result<&Snapshot> {
        self.state
            .snapshots
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::Invalid(format!("no snapshot {id}")))
    }

    /// Restores the files of snapshot `id` and reopens them as they were.
    ///
    /// Files are reopened in the snapshot's changelist, or the default
    /// changelist if it no longer exists. Exclusive files that are not opened
    /// any more are locked again. The current content is snapshotted first.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no such snapshot, its content is no longer
    /// cached, or a lock cannot be taken.
    pub async fn restore(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        id: u32,
    ) -> Result<Vec<DepotPath>> {
        let snapshot = self.snapshot_by_id(id)?.clone();
        let paths: Vec<DepotPath> = snapshot.files.iter().map(|f| f.path.clone()).collect();
        self.refresh_typemap(conn).await?;
        let relock: Vec<DepotPath> = paths
            .iter()
            .filter(|p| self.state.pending.action(p).is_none() && self.is_exclusive(p))
            .cloned()
            .collect();
        self.lock(conn, &relock).await?;
        for file in &snapshot.files {
            if self.state.pending.action(&file.path).is_none() {
                self.check_overwrite(&file.path, file.content.map(|v| v.content))?;
            }
        }
        self.auto_snapshot(cache, &paths, &format!("restoring snapshot {id}"))
            .await?;
        let cl = if self.state.pending.changelist(snapshot.cl).is_some() {
            snapshot.cl
        } else {
            LocalClId::DEFAULT
        };
        let bytes = snapshot
            .files
            .iter()
            .filter_map(|f| f.content.map(|v| v.size))
            .sum();
        let mut meter = cache.meter(Phase::Write, snapshot.files.len() as u64, Some(bytes));
        for file in &snapshot.files {
            meter.add(1, file.content.map_or(0, |v| v.size));
            let abs = self.local_path(&file.path)?;
            match file.content {
                Some(v) => {
                    self.write(&file.path, &cache.read(&v.content).await?, v.executable)?;
                    set_read_only(&abs, false)?;
                }
                None => remove_file(&abs, &self.root)?,
            }
            let splittable = file.action == OpenAction::Edit && self.splittable(&file.path);
            self.state
                .pending
                .open(file.path.clone(), file.action, cl, splittable)?;
        }
        let moves = self.moves_of(&snapshot);
        self.state.moves.retain(|to, _| !paths.contains(to));
        self.state.moves.extend(moves);
        self.save()?;
        Ok(paths)
    }

    /// Returns the opened moves among the files of `snapshot`, as in
    /// [`State::moves`](kelp_workspace::State::moves).
    ///
    /// Snapshots taken before Kelp recorded their moves pair the halves of
    /// a move only if they hold just one.
    fn moves_of(&self, snapshot: &Snapshot) -> BTreeMap<DepotPath, DepotPath> {
        let moves = match self.state.snapshot_moves.get(&snapshot.id) {
            Some(moves) => moves.clone(),
            None => {
                fn with(snapshot: &Snapshot, action: OpenAction) -> Vec<&DepotPath> {
                    snapshot
                        .files
                        .iter()
                        .filter(|f| f.action == action)
                        .map(|f| &f.path)
                        .collect()
                }
                let to = with(snapshot, OpenAction::MoveTo);
                let from = with(snapshot, OpenAction::MoveFrom);
                match (&to[..], &from[..]) {
                    ([to], [from]) => BTreeMap::from([((*to).clone(), (*from).clone())]),
                    _ => BTreeMap::new(),
                }
            }
        };
        // Both halves must be opened as such now.
        moves
            .into_iter()
            .filter(|(to, from)| {
                self.state.pending.action(to) == Some(OpenAction::MoveTo)
                    && self.state.pending.action(from) == Some(OpenAction::MoveFrom)
            })
            .collect()
    }

    /// Deletes snapshot `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if there is no such snapshot.
    pub fn delete_snapshot(&mut self, id: u32) -> Result<()> {
        self.snapshot_by_id(id)?;
        self.state.snapshots.retain(|s| s.id != id);
        self.state.snapshot_moves.remove(&id);
        self.save()
    }

    /// Returns the files that differ between two sides. When one side is
    /// [`SnapshotSide::Working`], the files of the snapshot on the other side
    /// are compared with the disk.
    ///
    /// # Errors
    ///
    /// Returns an error if a snapshot does not exist or its content is no
    /// longer cached.
    pub async fn diff_snapshots(
        &self,
        cache: &ContentCache,
        old: SnapshotSide,
        new: SnapshotSide,
    ) -> Result<Vec<FileDiff>> {
        let mut paths = Vec::new();
        for side in [old, new] {
            if let SnapshotSide::Snapshot(id) = side {
                paths.extend(
                    self.snapshot_by_id(id)?
                        .files
                        .iter()
                        .map(|f| f.path.clone()),
                );
            }
        }
        paths.sort();
        paths.dedup();
        let mut out = Vec::new();
        for path in paths {
            let (a, b) = (
                self.side_content(cache, old, &path).await?,
                self.side_content(cache, new, &path).await?,
            );
            if a != b {
                out.push(FileDiff {
                    path,
                    old: a,
                    new: b,
                });
            }
        }
        Ok(out)
    }

    async fn side_content(
        &self,
        cache: &ContentCache,
        side: SnapshotSide,
        path: &DepotPath,
    ) -> Result<Option<Vec<u8>>> {
        match side {
            SnapshotSide::Working => match fs::read(self.local_path(path)?) {
                Ok(data) => Ok(Some(data)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            SnapshotSide::Snapshot(id) => {
                let file = self
                    .snapshot_by_id(id)?
                    .files
                    .iter()
                    .find(|f| f.path == *path);
                match file.and_then(|f| f.content) {
                    Some(v) => Ok(Some(cache.read(&v.content).await?)),
                    None => Ok(None),
                }
            }
        }
    }
}

/// Renders a unified diff of `diff`, or a note for binary content.
pub fn render_diff(diff: &FileDiff) -> String {
    let text = |b: &Option<Vec<u8>>| match b {
        Some(bytes) => std::str::from_utf8(bytes).ok().map(str::to_owned),
        None => Some(String::new()),
    };
    match (text(&diff.old), text(&diff.new)) {
        (Some(old), Some(new)) => similar::TextDiff::from_lines(&old, &new)
            .unified_diff()
            .header(
                &format!("a{}", &diff.path.as_str()[1..]),
                &format!("b{}", &diff.path.as_str()[1..]),
            )
            .to_string(),
        _ => format!("Binary file {} differs\n", diff.path),
    }
}
