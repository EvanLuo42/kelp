//! Workspace operations as serializable values.
//!
//! The CLI either runs an [`Op`] in its own process or sends it to kelpd,
//! which runs it the same way through [`Session::execute`]. Engine and DCC
//! integrations reach the same operations through the C SDK.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kelp_core::{DepotPath, LocalClId};
use kelp_workspace::{META_DIR, OpenAction, Snapshot};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::progress::{Meter, Phase, Progress, Quiet};
use crate::{
    CleanReport, Connection, Error, FileDiff, FileReport, Home, MergeReport, Projector,
    ResolveMode, ResolveOutcome, Result, SnapshotSide, Status, SyncReport, Workspace,
};

const LOCK_WAIT: Duration = Duration::from_secs(60);
const LOCK_POLL: Duration = Duration::from_millis(50);
/// How long to wait for the workspace lock before saying so.
const LOCK_NOTICE: Duration = Duration::from_millis(500);

/// A workspace operation. Paths are absolute file system paths.
///
/// kelpd of one version executes a `Shutdown` sent by a client of another,
/// so `Shutdown` must keep its variant index: add new variants after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Sync {
        at: Option<u64>,
    },
    /// Syncs the files at or below `paths`, or all files if it is empty,
    /// to `rev`, or to the head.
    SyncTo {
        rev: Option<Revision>,
        paths: Vec<PathBuf>,
    },
    Status,
    Reconcile,
    Open {
        paths: Vec<PathBuf>,
        action: OpenAction,
        cl: LocalClId,
    },
    Revert {
        paths: Vec<PathBuf>,
    },
    /// Moves a file, or every file below a directory, keeping history.
    Move {
        from: PathBuf,
        to: PathBuf,
        cl: LocalClId,
    },
    Resolve {
        paths: Vec<PathBuf>,
        mode: ResolveMode,
    },
    Submit {
        cl: LocalClId,
        message: Option<String>,
        /// Issue keys the changelist fixes.
        fixes: Vec<String>,
    },
    Shelve {
        cl: LocalClId,
    },
    Unshelve {
        shelf: u64,
        cl: Option<LocalClId>,
    },
    Merge {
        from: Option<DepotPath>,
    },
    /// Opens the changes of one changelist; see [`Workspace::cherry_pick`].
    Pick {
        from: Option<DepotPath>,
        cl: u64,
    },
    /// Opens the reverse of a changelist; see [`Workspace::undo`].
    Undo {
        cl: u64,
    },
    Unlock {
        paths: Vec<PathBuf>,
        force: bool,
    },
    ClNew {
        description: String,
    },
    ClList,
    ClDelete {
        cl: LocalClId,
    },
    ClMove {
        cl: LocalClId,
        paths: Vec<PathBuf>,
    },
    Hunks {
        path: PathBuf,
    },
    AssignHunk {
        path: PathBuf,
        hunk: usize,
        to: LocalClId,
    },
    Snapshot {
        cl: LocalClId,
        message: String,
    },
    Snapshots,
    Restore {
        id: u32,
    },
    DeleteSnapshot {
        id: u32,
    },
    DiffSnapshots {
        old: SnapshotSide,
        new: SnapshotSide,
    },
    /// Shows how files differ from the versions synced; all changed files
    /// if `paths` is empty.
    Diff {
        paths: Vec<PathBuf>,
    },
    /// Puts unopened files back as synced and deletes untracked files.
    Clean {
        paths: Vec<PathBuf>,
        dry_run: bool,
    },
    /// Reports the state of files for editors.
    Inspect {
        paths: Vec<PathBuf>,
    },
    /// Mounts the projected workspace at the operation's directory, keeping
    /// its files in `upper`. Only kelpd runs this.
    Mount {
        upper: PathBuf,
    },
    /// Unmounts the projected workspace at the operation's directory. Only
    /// kelpd runs this.
    Unmount,
    /// Asks kelpd whether it is alive.
    Ping,
    /// Asks kelpd to exit.
    Shutdown,
}

/// A point in a stream's history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Revision {
    Change(u64),
    /// A label, which must name the workspace's stream.
    Label(String),
}

/// A pending changelist as listed by [`Op::ClList`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelistInfo {
    pub id: LocalClId,
    pub description: String,
    pub files: usize,
    pub shelf: Option<u64>,
}

/// A hunk as listed by [`Op::Hunks`], numbered from 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkInfo {
    pub number: usize,
    pub owner: LocalClId,
    /// Lines of the base replaced by the hunk, from 1.
    pub first_line: usize,
    pub last_line: usize,
    pub preview: String,
}

/// The result of an [`Op`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Done,
    Synced(SyncReport),
    Status {
        status: Status,
        changelists: Vec<ChangelistInfo>,
        have_cl: u64,
    },
    Opened(Vec<(DepotPath, OpenAction)>),
    Reverted(Vec<DepotPath>),
    Resolved(Vec<(DepotPath, ResolveOutcome)>),
    Submitted(u64),
    Shelved(u64),
    Unshelved {
        cl: LocalClId,
        opened: Vec<(DepotPath, OpenAction)>,
    },
    Merged(MergeReport),
    Changelist(LocalClId),
    Changelists(Vec<ChangelistInfo>),
    Hunks(Vec<HunkInfo>),
    Snapshot(u32),
    Snapshots(Vec<Snapshot>),
    Restored(Vec<DepotPath>),
    Diff(Vec<FileDiff>),
    Files(Vec<FileReport>),
    Cleaned(CleanReport),
}

/// Runs operations for one user, reusing server connections between them.
pub struct Session {
    home: Home,
    connections: Mutex<HashMap<String, Connection>>,
    projectors: std::sync::Mutex<HashMap<PathBuf, Arc<dyn Projector>>>,
}

impl Session {
    /// Creates a session for the user whose Kelp home is `home`.
    pub fn new(home: Home) -> Self {
        Self {
            home,
            connections: Mutex::new(HashMap::new()),
            projectors: std::sync::Mutex::default(),
        }
    }

    /// Returns the user's Kelp home.
    pub fn home(&self) -> &Home {
        &self.home
    }

    /// Registers the projector of the projected workspace mounted at `root`,
    /// or removes it if `None`.
    pub fn set_projector(&self, root: &Path, projector: Option<Arc<dyn Projector>>) {
        let mut projectors = self.projectors.lock().expect("projector lock");
        match projector {
            Some(p) => projectors.insert(root.to_owned(), p),
            None => projectors.remove(root),
        };
    }

    /// Returns an open connection to `server`, reconnecting if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the user is not logged in or the server is unreachable.
    pub async fn connection(&self, server: &str) -> Result<Connection> {
        let mut connections = self.connections.lock().await;
        if let Some(conn) = connections.get(server).filter(|c| !c.is_closed()) {
            return Ok(conn.clone());
        }
        let conn = self.home.connect(server).await?;
        connections.insert(server.to_owned(), conn.clone());
        Ok(conn)
    }

    /// Runs `op` in the workspace containing `cwd`.
    ///
    /// The workspace is locked against other Kelp processes for the duration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoWorkspace`] outside a workspace, [`Error::Busy`] if the
    /// workspace stays locked, or the error of the operation.
    pub async fn execute(&self, cwd: &Path, op: Op) -> Result<Outcome> {
        self.execute_with(cwd, op, Arc::new(Quiet)).await
    }

    /// Like [`execute`](Self::execute), reporting the progress of long
    /// operations to `progress`.
    ///
    /// # Errors
    ///
    /// As for [`execute`](Self::execute).
    pub async fn execute_with(
        &self,
        cwd: &Path,
        op: Op,
        progress: Arc<dyn Progress>,
    ) -> Result<Outcome> {
        match op {
            Op::Ping | Op::Shutdown => return Ok(Outcome::Done),
            Op::Mount { .. } | Op::Unmount => {
                return Err(Error::Invalid(
                    "projected workspaces are mounted by kelpd".into(),
                ));
            }
            _ => {}
        }
        let root = Workspace::discover(cwd)?.root().to_owned();
        let mut lock = workspace_lock(&root)?;
        let mut waited = Duration::ZERO;
        let mut waiting = None;
        // The guard that ends the wait is the one held: letting go of it to
        // take it again would let another process slip in between.
        let _guard = loop {
            if let Ok(guard) = lock.try_write() {
                break guard;
            }
            if waited >= LOCK_WAIT {
                return Err(Error::Busy);
            }
            if waited >= LOCK_NOTICE && waiting.is_none() {
                waiting = Some(Meter::start(progress.clone(), Phase::Wait, 1, None));
            }
            tokio::time::sleep(LOCK_POLL).await;
            waited += LOCK_POLL;
        };
        if let Some(mut meter) = waiting {
            meter.add(1, 0);
        }
        let mut ws = Workspace::open(&root)?;
        if let Some(projector) = self.projectors.lock().expect("projector lock").get(&root) {
            ws.set_projector(projector.clone());
        }
        self.run(&mut ws, op, progress).await
    }

    async fn run(
        &self,
        ws: &mut Workspace,
        op: Op,
        progress: Arc<dyn Progress>,
    ) -> Result<Outcome> {
        let cache = self.home.cache().with_progress(progress);
        let server = ws.config().server.clone();
        Ok(match op {
            Op::Sync { at } => Outcome::Synced(
                ws.sync(&self.connection(&server).await?, &cache, at)
                    .await?,
            ),
            Op::SyncTo { rev, paths } => {
                let conn = self.connection(&server).await?;
                let at = match rev {
                    None => None,
                    Some(Revision::Change(cl)) => Some(cl),
                    Some(Revision::Label(name)) => {
                        let request = kelp_proto::Request::GetLabel { name: name.clone() };
                        let kelp_proto::Reply::Label(label) = conn.call(&request).await? else {
                            return Err(Error::UnexpectedReply);
                        };
                        let stream = DepotPath::parse(&label.stream)?;
                        if stream != *ws.stream() {
                            return Err(Error::Invalid(format!(
                                "label {name} is of {stream}, not {}",
                                ws.stream()
                            )));
                        }
                        Some(label.cl)
                    }
                };
                let filter = paths
                    .iter()
                    .map(|p| ws.depot_path_of(p))
                    .collect::<Result<Vec<_>>>()?;
                Outcome::Synced(ws.sync_paths(&conn, &cache, at, &filter).await?)
            }
            Op::Status => {
                let status = ws.refresh_status(None)?;
                status_outcome(ws, status)
            }
            Op::Reconcile => Outcome::Opened(ws.reconcile(&self.connection(&server).await?).await?),
            Op::Open { paths, action, cl } => {
                let depot = ws.expand(&paths, action == OpenAction::Add)?;
                let depot: Vec<_> = match action {
                    OpenAction::Add => depot
                        .into_iter()
                        .filter(|p| !ws.state().have.contains_key(p))
                        .collect(),
                    _ => depot,
                };
                if depot.is_empty() {
                    return Err(Error::Invalid("no matching files".into()));
                }
                ws.open_files(&self.connection(&server).await?, &depot, action, cl)
                    .await?;
                Outcome::Opened(depot.into_iter().map(|p| (p, action)).collect())
            }
            Op::Revert { paths } => {
                let opened: Vec<DepotPath> = ws
                    .expand(&paths, true)?
                    .into_iter()
                    .filter(|p| ws.state().pending.action(p).is_some())
                    .collect();
                ws.revert(&self.connection(&server).await?, &cache, &opened)
                    .await?;
                Outcome::Reverted(opened)
            }
            Op::Move { from, to, cl } => {
                let (from, to) = (ws.depot_path_of(&from)?, ws.depot_path_of(&to)?);
                let files: Vec<DepotPath> = ws
                    .state()
                    .have
                    .keys()
                    .filter(|p| p.starts_with(&from))
                    .cloned()
                    .collect();
                if files.is_empty() {
                    return Err(Error::NotTracked(from));
                }
                let conn = self.connection(&server).await?;
                let mut opened = Vec::new();
                for file in files {
                    let target = match file == from {
                        true => to.clone(),
                        false => {
                            let rel = &file.as_str()[from.as_str().len() + 1..];
                            to.join(rel)?
                        }
                    };
                    ws.move_file(&conn, &file, &target, cl).await?;
                    opened.push((file, kelp_workspace::OpenAction::MoveFrom));
                    opened.push((target, kelp_workspace::OpenAction::MoveTo));
                }
                Outcome::Opened(opened)
            }
            Op::Resolve { paths, mode } => {
                let paths = if paths.is_empty() {
                    Vec::new()
                } else {
                    ws.expand(&paths, false)?
                };
                Outcome::Resolved(
                    ws.resolve(&self.connection(&server).await?, &cache, &paths, mode)
                        .await?,
                )
            }
            Op::Submit { cl, message, fixes } => Outcome::Submitted(
                ws.submit_fixing(
                    &self.connection(&server).await?,
                    &cache,
                    cl,
                    message,
                    &fixes,
                )
                .await?,
            ),
            Op::Shelve { cl } => Outcome::Shelved(
                ws.shelve(&self.connection(&server).await?, &cache, cl)
                    .await?,
            ),
            Op::Unshelve { shelf, cl } => {
                let (cl, opened) = ws
                    .unshelve(&self.connection(&server).await?, &cache, shelf, cl)
                    .await?;
                Outcome::Unshelved { cl, opened }
            }
            Op::Merge { from } => Outcome::Merged(
                ws.merge(&self.connection(&server).await?, &cache, from)
                    .await?,
            ),
            Op::Pick { from, cl } => Outcome::Merged(
                ws.cherry_pick(&self.connection(&server).await?, &cache, from, cl)
                    .await?,
            ),
            Op::Undo { cl } => Outcome::Merged(
                ws.undo(&self.connection(&server).await?, &cache, cl)
                    .await?,
            ),
            Op::Unlock { paths, force } => {
                let paths = ws.expand(&paths, false)?;
                ws.unlock(&self.connection(&server).await?, &paths, force)
                    .await?;
                Outcome::Done
            }
            Op::ClNew { description } => {
                let id = ws.pending_mut().create(description);
                ws.save()?;
                Outcome::Changelist(id)
            }
            Op::ClList => Outcome::Changelists(changelists(ws)),
            Op::ClDelete { cl } => {
                ws.pending_mut().delete(cl)?;
                ws.save()?;
                Outcome::Done
            }
            Op::ClMove { cl, paths } => {
                for path in ws.expand(&paths, false)? {
                    ws.pending_mut().reopen(&path, cl)?;
                }
                ws.save()?;
                Outcome::Done
            }
            Op::Hunks { path } => {
                let path = single(ws, path)?;
                let hunks = ws.hunks(&cache, &path).await?;
                Outcome::Hunks(
                    hunks
                        .into_iter()
                        .enumerate()
                        .map(|(i, (h, owner))| HunkInfo {
                            number: i + 1,
                            owner,
                            first_line: h.base.start + 1,
                            last_line: h.base.end,
                            preview: h
                                .new_lines
                                .first()
                                .map_or("(deletion)".into(), |l| l.trim_end().to_owned()),
                        })
                        .collect(),
                )
            }
            Op::AssignHunk { path, hunk, to } => {
                let path = single(ws, path)?;
                let hunks = ws.hunks(&cache, &path).await?;
                let (h, _) = hunks
                    .get(hunk.wrapping_sub(1))
                    .ok_or_else(|| Error::Invalid(format!("no hunk {hunk}")))?;
                ws.pending_mut().assign_hunk(&path, h.anchor, to)?;
                ws.save()?;
                Outcome::Done
            }
            Op::Snapshot { cl, message } => {
                Outcome::Snapshot(ws.snapshot(&cache, cl, &message).await?)
            }
            Op::Snapshots => Outcome::Snapshots(ws.state().snapshots.clone()),
            Op::Restore { id } => Outcome::Restored(
                ws.restore(&self.connection(&server).await?, &cache, id)
                    .await?,
            ),
            Op::DeleteSnapshot { id } => {
                ws.delete_snapshot(id)?;
                Outcome::Done
            }
            Op::Inspect { paths } => {
                let paths: Vec<DepotPath> = paths
                    .iter()
                    .map(|p| ws.depot_path_of(p))
                    .collect::<Result<_>>()?;
                Outcome::Files(ws.inspect(&self.connection(&server).await?, &paths).await?)
            }
            Op::Diff { paths } => {
                let filter = match paths.is_empty() {
                    true => Vec::new(),
                    false => ws.expand(&paths, true)?,
                };
                let conn = self.connection(&server).await?;
                Outcome::Diff(ws.diff(&conn, &cache, &filter).await?)
            }
            Op::Clean { paths, dry_run } => {
                let filter = match paths.is_empty() {
                    true => Vec::new(),
                    false => ws.expand(&paths, true)?,
                };
                let conn = self.connection(&server).await?;
                Outcome::Cleaned(ws.clean(&conn, &cache, &filter, dry_run).await?)
            }
            Op::DiffSnapshots { old, new } => {
                Outcome::Diff(ws.diff_snapshots(&cache, old, new).await?)
            }
            Op::Ping | Op::Shutdown | Op::Mount { .. } | Op::Unmount => Outcome::Done,
        })
    }
}

/// Runs `f` holding the lock that [`Session::execute`] takes on the workspace
/// at `root`, or returns `None` at once if another operation holds it.
///
/// # Errors
///
/// Returns an error if the lock file cannot be opened.
pub fn if_unlocked<T>(root: &Path, f: impl FnOnce() -> T) -> Result<Option<T>> {
    let mut lock = workspace_lock(root)?;
    let Ok(_guard) = lock.try_write() else {
        return Ok(None);
    };
    Ok(Some(f()))
}

fn workspace_lock(root: &Path) -> Result<fd_lock::RwLock<std::fs::File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(META_DIR).join("lock"))?;
    Ok(fd_lock::RwLock::new(file))
}

fn single(ws: &Workspace, path: PathBuf) -> Result<DepotPath> {
    let mut paths = ws.expand(&[path], false)?;
    match paths.len() {
        1 => Ok(paths.remove(0)),
        _ => Err(Error::Invalid("name exactly one file".into())),
    }
}

/// Returns the [`Outcome::Status`] for `ws` given its `status`.
pub fn status_outcome(ws: &Workspace, status: Status) -> Outcome {
    Outcome::Status {
        status,
        changelists: changelists(ws),
        have_cl: ws.state().have_cl,
    }
}

fn changelists(ws: &Workspace) -> Vec<ChangelistInfo> {
    ws.state()
        .pending
        .changelists()
        .map(|(id, cl)| ChangelistInfo {
            id,
            description: cl.description.clone(),
            files: ws
                .state()
                .pending
                .opened()
                .filter(|(_, _, owner)| *owner == id)
                .count(),
            shelf: cl.shelf.map(|s| s.0),
        })
        .collect()
}
