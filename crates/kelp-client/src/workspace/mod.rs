mod artifact;
mod files;
mod inspect;
mod merge;
mod open;
mod resolve;
mod snapshot;
mod submit;
mod sync;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kelp_core::time::{Clock, SystemClock};

use kelp_core::typemap::{FileKind, FileType, LockPolicy, Typemap};
use kelp_core::{ContentId, DepotPath, LocalClId};
use kelp_proto::rpc::FileInfo;
use kelp_proto::{Reply, Request};
use kelp_workspace::ignore::is_meta_path;
use kelp_workspace::{
    Config, FileVersion, HaveEntry, Ignore, META_DIR, OpenAction, PendingChanges, State, View,
};

pub use artifact::ArtifactUpdate;
pub use files::{AnnotatedLine, CleanReport, annotate, read_file};
pub use inspect::FileReport;
pub(crate) use submit::PARALLEL_INGESTS;

pub use merge::MergeReport;
pub use resolve::{ResolveMode, ResolveOutcome};
pub use snapshot::{Described, FileDiff, SnapshotSide, render_diff};
pub use sync::{SkipReason, SyncReport};

use crate::fsutil::{content_of, entry_matches, file_matches, mtime_trusted};
use crate::index::DiskEntry;
use crate::{Connection, Error, Result};

/// A workspace on disk: a directory containing a [`META_DIR`] and the files
/// synced into it.
pub struct Workspace {
    root: PathBuf,
    config: Config,
    stream: DepotPath,
    view: View,
    typemap: Option<Typemap>,
    imports: Vec<MountedImport>,
    state: State,
    clock: Arc<dyn Clock>,
    projector: Option<Arc<dyn Projector>>,
    /// When the state was last saved, by the clock of the file system; see
    /// [`State::saved_at`]. Every modification time in the saved state was
    /// recorded before then.
    saved_at: AtomicU64,
}

/// An import of the workspace's stream, with the typemap of its source
/// depot, which decides the types of the files below it.
struct MountedImport {
    mount: DepotPath,
    from: String,
    typemap: Option<Typemap>,
}

/// A file as a projected workspace shows it before it is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectedFile {
    pub version: FileVersion,
    pub read_only: bool,
}

/// Updates the files a projected workspace shows, without downloading them.
///
/// kelpd implements this for the workspaces it has mounted.
pub trait Projector: Send + Sync {
    /// Makes the workspace path `local` show `file`, or nothing if `None`,
    /// and returns `true`; or returns `false` and leaves the path alone if it
    /// was modified locally.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection cannot be updated.
    fn project(&self, local: &str, file: Option<ProjectedFile>) -> Result<bool>;
}

/// The outcome of [`Workspace::status`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    /// Opened files, with their action and changelist.
    pub opened: Vec<(DepotPath, OpenAction, LocalClId)>,
    /// Opened files that need a resolve before they can be submitted.
    pub unresolved: Vec<DepotPath>,
    /// Tracked files changed on disk but not opened.
    pub modified: Vec<DepotPath>,
    /// Files on disk that the workspace does not track and are not ignored.
    pub added: Vec<DepotPath>,
    /// Tracked files missing from disk but not opened for delete.
    pub deleted: Vec<DepotPath>,
}

impl Status {
    /// Returns `true` if there are changes on disk that are not opened.
    pub fn has_unopened_changes(&self) -> bool {
        !(self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty())
    }
}

impl Workspace {
    /// Creates a workspace rooted at `root` with `config`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WorkspaceExists`] if `root` already holds a workspace,
    /// or an error if the configuration is invalid or cannot be written.
    pub fn create(root: &Path, config: Config) -> Result<Self> {
        if root.join(META_DIR).exists() {
            return Err(Error::WorkspaceExists(root.to_owned()));
        }
        if config.name.is_empty() {
            return Err(Error::Invalid("a workspace needs a name".into()));
        }
        config.validate()?;
        fs::create_dir_all(root)?;
        config.save(root)?;
        State::default().save(root)?;
        Self::open(root)
    }

    /// Opens the workspace rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace files are missing or invalid.
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let config = Config::load(&root)?;
        let state = State::load(&root)?;
        let saved_at = AtomicU64::new(State::saved_at(&root).unwrap_or(0));
        Ok(Self {
            stream: config.stream_path()?,
            view: config.view()?,
            typemap: parse_typemap(state.typemap.as_deref())?,
            imports: Vec::new(),
            state,
            config,
            root,
            clock: Arc::new(SystemClock),
            projector: None,
            saved_at,
        })
    }

    /// Sets the projector of a projected workspace.
    pub fn set_projector(&mut self, projector: Arc<dyn Projector>) {
        self.projector = Some(projector);
    }

    /// Replaces the clock used to timestamp snapshots.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Opens the workspace containing `dir`, searching upwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoWorkspace`] if no ancestor of `dir` is a workspace root.
    pub fn discover(dir: &Path) -> Result<Self> {
        let dir = fs::canonicalize(dir).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                Error::Invalid(format!("{} does not exist", dir.display()))
            }
            _ => e.into(),
        })?;
        let root = dir
            .ancestors()
            .find(|d| Config::exists(d))
            .ok_or(Error::NoWorkspace)?;
        Self::open(root)
    }

    /// Returns the workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the workspace configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Points the workspace at the server's new address `server`.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration cannot be written.
    pub fn set_server(&mut self, server: &str) -> Result<()> {
        self.config.server = server.to_owned();
        Ok(self.config.save(&self.root)?)
    }

    /// Returns the stream the workspace is bound to.
    pub fn stream(&self) -> &DepotPath {
        &self.stream
    }

    /// Returns the workspace state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Returns the pending changelists for modification. Call
    /// [`Workspace::save`] afterwards to persist changes.
    pub fn pending_mut(&mut self) -> &mut PendingChanges {
        &mut self.state.pending
    }

    /// Persists the workspace state.
    ///
    /// # Errors
    ///
    /// Returns an error if the state cannot be written.
    pub fn save(&self) -> Result<()> {
        self.state.save(&self.root)?;
        let saved_at = State::saved_at(&self.root).unwrap_or(0);
        self.saved_at.store(saved_at, Ordering::Relaxed);
        Ok(())
    }

    /// Returns a time, by the clock of the file system, that every
    /// modification time the workspace trusts was recorded before.
    ///
    /// A recorded time is only trusted if it is older than this; see
    /// [`mtime_trusted`](crate::fsutil::mtime_trusted).
    fn recorded(&self) -> u64 {
        self.saved_at.load(Ordering::Relaxed)
    }

    /// Returns the type of `path` under the depot's typemap as last fetched,
    /// or, below an import, under the typemap of the import's source depot.
    pub fn file_type(&self, path: &DepotPath) -> FileType {
        let mounted = self
            .imports
            .iter()
            .find(|i| path.starts_with(&i.mount) && *path != i.mount);
        let (typemap, rel) = match mounted {
            Some(i) => {
                let rest = stream_relative(&i.mount, path);
                let rel = match i.from.is_empty() {
                    true => rest,
                    false => format!("{}/{rest}", i.from),
                };
                (&i.typemap, rel)
            }
            None => (&self.typemap, self.stream_relative(path)),
        };
        match typemap {
            Some(t) => t.resolve(&rel),
            None => FileType::default(),
        }
    }

    /// Returns the absolute path of the workspace file `path` maps to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutsideView`] if the view does not map `path`.
    pub fn local_path(&self, path: &DepotPath) -> Result<PathBuf> {
        Ok(self.abs(&self.view_map(path)?))
    }

    /// Returns the depot path of a file system path inside the workspace.
    ///
    /// Relative paths are resolved against the workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutsideView`] if the path is outside the workspace or
    /// not mapped by its view.
    pub fn depot_path_of(&self, path: &Path) -> Result<DepotPath> {
        let local = self.local_of(path)?;
        self.view.to_depot(&local).ok_or(Error::OutsideView(local))
    }

    /// Expands file system paths, which may name directories, into depot paths.
    ///
    /// Directories expand to the tracked and opened files below them, and also
    /// to the untracked, non-ignored files below them if `untracked` is set.
    ///
    /// # Errors
    ///
    /// Returns an error if a path is outside the workspace view.
    pub fn expand(&self, paths: &[PathBuf], untracked: bool) -> Result<Vec<DepotPath>> {
        let mut out = BTreeSet::new();
        for path in paths {
            let local = self.local_of(path)?;
            if !self.abs(&local).is_dir() {
                out.insert(
                    self.view
                        .to_depot(&local)
                        .ok_or(Error::OutsideView(local))?,
                );
                continue;
            }
            let prefix = if local.is_empty() {
                String::new()
            } else {
                format!("{local}/")
            };
            let below = |p: &DepotPath| self.view.map(p).is_some_and(|l| l.starts_with(&prefix));
            out.extend(self.state.have.keys().filter(|p| below(p)).cloned());
            out.extend(
                self.state
                    .pending
                    .opened()
                    .map(|(p, _, _)| p)
                    .filter(|p| below(p))
                    .cloned(),
            );
            if untracked {
                for file in self.walk(&local)? {
                    out.extend(self.view.to_depot(&file));
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Compares the disk with what the workspace has and has opened.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace cannot be read.
    pub fn status(&self) -> Result<Status> {
        Ok(self.status_of(self.walk_entries("")?)?.0)
    }

    /// Compares an index of the disk with what the workspace has and has
    /// opened, without listing directories.
    ///
    /// # Errors
    ///
    /// Returns an error if a file whose metadata changed cannot be hashed.
    pub fn status_indexed(&self, index: &crate::DiskIndex) -> Result<Status> {
        Ok(self.status_of(index_entries(index))?.0)
    }

    /// Like [`status`](Self::status), or [`status_indexed`](Self::status_indexed)
    /// given an `index`, and records the modification times of files that
    /// hashing found unchanged, so that later statuses need not hash them
    /// again.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace cannot be read or its state written.
    pub fn refresh_status(&mut self, index: Option<&crate::DiskIndex>) -> Result<Status> {
        let files = match index {
            Some(index) => index_entries(index),
            None => self.walk_entries("")?,
        };
        let (status, unchanged) = self.status_of(files)?;
        if !unchanged.is_empty() {
            for (path, mtime) in unchanged {
                if let Some(have) = self.state.have.get_mut(&path) {
                    have.mtime = mtime;
                }
            }
            self.save()?;
        }
        Ok(status)
    }

    /// Returns the status of `files`, and the tracked files among them that
    /// had to be hashed and are unchanged, with the modification times to
    /// trust for them from now on.
    fn status_of(
        &self,
        files: Vec<(String, DiskEntry)>,
    ) -> Result<(Status, Vec<(DepotPath, u64)>)> {
        let mut status = Status {
            opened: self
                .state
                .pending
                .opened()
                .map(|(p, a, cl)| (p.clone(), a, cl))
                .collect(),
            unresolved: self.state.resolves.keys().cloned().collect(),
            ..Status::default()
        };
        let mut seen = BTreeSet::new();
        let mut unchanged = Vec::new();
        let recorded = self.recorded();
        for (local, entry) in files {
            let Some(path) = self.view.to_depot(&local) else {
                continue;
            };
            seen.insert(path.clone());
            if self.state.pending.action(&path).is_some() {
                continue;
            }
            match self.state.have.get(&path) {
                Some(h) if !entry_matches(&self.abs(&local), h, entry, recorded)? => {
                    status.modified.push(path)
                }
                // Hashed and unchanged. Saving the time, even if it is the
                // one recorded, lets the next status trust it.
                Some(h) if !mtime_trusted(h, entry, recorded) && !racy(entry.mtime) => {
                    unchanged.push((path, entry.mtime))
                }
                Some(_) => {}
                None => status.added.push(path),
            }
        }
        for (path, h) in &self.state.have {
            if seen.contains(path) || self.state.pending.action(path).is_some() {
                continue;
            }
            // Listings leave out ignored files and artifact directories, but
            // a tracked file there, say one a newly synced .kelpignore
            // covers, is still tracked and not deleted.
            let abs = self.abs(&h.local);
            let entry = match fs::symlink_metadata(&abs) {
                Ok(meta) if meta.is_file() => DiskEntry::of(&meta),
                // Missing, or something else is in its place.
                _ => {
                    status.deleted.push(path.clone());
                    continue;
                }
            };
            if !entry_matches(&abs, h, entry, recorded)? {
                status.modified.push(path.clone());
            } else if !mtime_trusted(h, entry, recorded) && !racy(entry.mtime) {
                unchanged.push((path.clone(), entry.mtime));
            }
        }
        status.modified.sort();
        Ok((status, unchanged))
    }

    /// Fetches the depot's typemap from the server.
    async fn refresh_typemap(&mut self, conn: &Connection) -> Result<()> {
        let depot = self.stream.depot().to_owned();
        let Reply::Text(source) = conn.call(&Request::GetTypemap { depot }).await? else {
            return Err(Error::UnexpectedReply);
        };
        self.typemap = parse_typemap(source.as_deref())?;
        self.state.typemap = source;
        let stream = self.stream.to_string();
        let Reply::Imports(imports) = conn.call(&Request::GetImports { stream }).await? else {
            return Err(Error::UnexpectedReply);
        };
        self.imports.clear();
        for import in imports {
            let source = DepotPath::parse(&import.source)
                .map_err(|e| Error::Invalid(format!("import source: {e}")))?;
            let depot = source.depot().to_owned();
            let Reply::Text(text) = conn.call(&Request::GetTypemap { depot }).await? else {
                return Err(Error::UnexpectedReply);
            };
            self.imports.push(MountedImport {
                mount: self
                    .stream
                    .join(&import.to)
                    .map_err(|e| Error::Invalid(format!("import path: {e}")))?,
                from: import.from,
                typemap: parse_typemap(text.as_deref())?,
            });
        }
        Ok(())
    }

    fn is_exclusive(&self, path: &DepotPath) -> bool {
        self.file_type(path).lock == LockPolicy::Exclusive
    }

    fn splittable(&self, path: &DepotPath) -> bool {
        let ty = self.file_type(path);
        ty.lock == LockPolicy::None && ty.kind != FileKind::Binary
    }

    /// Returns the workspace path `path` maps to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutsideView`] if the view does not map `path`, and
    /// [`Error::Invalid`] if it maps it into a directory named like
    /// [`META_DIR`], where Kelp keeps its own files.
    fn view_map(&self, path: &DepotPath) -> Result<String> {
        let local = self
            .view
            .map(path)
            .ok_or_else(|| Error::OutsideView(path.to_string()))?;
        if is_meta_path(&local) {
            return Err(reserved(path));
        }
        Ok(local)
    }

    fn stream_relative(&self, path: &DepotPath) -> String {
        stream_relative(&self.stream, path)
    }

    fn abs(&self, local: &str) -> PathBuf {
        if local.is_empty() {
            self.root.clone()
        } else {
            self.root.join(local)
        }
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(META_DIR).join("tmp")
    }

    /// Returns the workspace path of `path`, with `/` separators.
    fn local_of(&self, path: &Path) -> Result<String> {
        let abs = if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        };
        let abs = resolve_existing_prefix(&abs)?;
        let rel = abs
            .strip_prefix(&self.root)
            .map_err(|_| Error::OutsideView(path.display().to_string()))?;
        let parts: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Ok(parts.join("/"))
    }

    /// Returns the files below the workspace path `local` that the current
    /// `.kelpignore` does not exclude.
    fn walk(&self, local: &str) -> Result<Vec<String>> {
        Ok(self
            .walk_entries(local)?
            .into_iter()
            .map(|(l, _)| l)
            .collect())
    }

    /// Returns the files below the workspace path `local` that the current
    /// `.kelpignore` does not exclude, with their size and modification time.
    pub(crate) fn walk_entries(&self, local: &str) -> Result<Vec<(String, DiskEntry)>> {
        let ignore = Ignore::load(&self.root)?;
        let mut files = Vec::new();
        let start = self.abs(local);
        if !start.exists() {
            return Ok(files);
        }
        let walker = walkdir::WalkDir::new(start).sort_by_file_name().into_iter();
        for entry in walker.filter_entry(|e| {
            let rel = self.relative(e.path());
            rel.is_empty() || !(ignore.is_ignored(&rel) || self.in_artifact_dir(&rel))
        }) {
            let entry = entry.map_err(|e| Error::Io(e.into()))?;
            if entry.file_type().is_file() {
                let meta = entry.metadata().map_err(|e| Error::Io(e.into()))?;
                files.push((self.relative(entry.path()), DiskEntry::of(&meta)));
            }
        }
        Ok(files)
    }

    /// Returns the workspace path of a path below the root, with `/` separators.
    pub(crate) fn relative(&self, abs: &Path) -> String {
        let rel = abs.strip_prefix(&self.root).unwrap_or(abs);
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Records that the workspace file for `path` now holds `file`.
    fn record_have(&mut self, path: &DepotPath, file: FileVersion) -> Result<()> {
        self.record_have_observed(path, file, None)
    }

    /// Like [`record_have`](Self::record_have) for a file read earlier, whose
    /// size and modification time were `observed` then. If the file has
    /// changed since, or its modification time is too coarse to tell a later
    /// change apart, the time is not recorded, so the next status hashes the
    /// file instead of trusting it.
    ///
    /// A recorded time is only trusted once the state has been saved after
    /// it; see [`recorded`](Self::recorded).
    fn record_have_observed(
        &mut self,
        path: &DepotPath,
        file: FileVersion,
        observed: Option<DiskEntry>,
    ) -> Result<()> {
        let local = self.view_map(path)?;
        let now = DiskEntry::of(&fs::metadata(self.abs(&local))?);
        let trusted =
            now.size == file.size && observed.is_none_or(|o| o == now) && !racy(now.mtime);
        let mtime = if trusted { now.mtime } else { 0 };
        let entry = HaveEntry {
            local,
            content: file.content,
            size: file.size,
            executable: file.executable,
            mtime,
        };
        self.state.have.insert(path.clone(), entry);
        Ok(())
    }

    /// Returns an error unless the workspace file of `path` can be replaced
    /// or removed without losing changes: it is absent, still has the content
    /// the workspace recorded for it, or already has `target`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WouldOverwrite`] if the file has changes of its own.
    pub(super) fn check_overwrite(
        &self,
        path: &DepotPath,
        target: Option<ContentId>,
    ) -> Result<()> {
        let abs = self.local_path(path)?;
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if meta.is_file() {
            if let Some(have) = self.state.have.get(path)
                && file_matches(&abs, have, self.recorded())?
            {
                return Ok(());
            }
            if target.is_some_and(|t| content_of(&abs).is_ok_and(|c| c == t)) {
                return Ok(());
            }
        }
        Err(Error::WouldOverwrite(path.clone()))
    }

    fn have_version(&self, path: &DepotPath) -> Option<FileVersion> {
        self.state.have.get(path).map(|h| FileVersion {
            content: h.content,
            size: h.size,
            executable: h.executable,
        })
    }
}

/// The error for a file that maps into a directory reserved for Kelp.
pub(crate) fn reserved(path: &DepotPath) -> Error {
    Error::Invalid(format!(
        "{path} maps into a {META_DIR} directory, which Kelp reserves for its own files"
    ))
}

fn index_entries(index: &crate::DiskIndex) -> Vec<(String, DiskEntry)> {
    index.entries().map(|(l, e)| (l.to_owned(), *e)).collect()
}

fn parse_typemap(source: Option<&str>) -> Result<Option<Typemap>> {
    source
        .map(Typemap::parse)
        .transpose()
        .map_err(|e| Error::Invalid(format!("server typemap: {e}")))
}

pub(crate) fn version(f: FileInfo) -> FileVersion {
    FileVersion {
        content: f.content,
        size: f.size,
        executable: f.executable,
    }
}

/// `//depot/stream/a/b` relative to `//depot/stream` is `a/b`.
pub(crate) fn stream_relative(stream: &DepotPath, path: &DepotPath) -> String {
    path.segments()
        .skip(stream.segments().count())
        .collect::<Vec<_>>()
        .join("/")
}

/// Makes `path` canonical as far as it exists and normalises the rest
/// lexically, so that paths of deleted files still resolve.
fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_owned();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_owned());
                existing = parent.to_owned();
            }
            _ => break,
        }
    }
    let mut out = fs::canonicalize(&existing)?;
    for name in rest.into_iter().rev() {
        match Path::new(&name).components().next() {
            Some(Component::ParentDir) => {
                out.pop();
            }
            Some(Component::CurDir) | None => {}
            Some(_) => out.push(name),
        }
    }
    Ok(out)
}

/// Returns `true` if a modification time of `mtime` nanoseconds is not worth
/// recording now: it is unknown (0), not in the past, or whole seconds, as
/// file systems such as FAT and HFS+ record, within two seconds of now, so
/// a later write could still leave it unchanged.
///
/// Times that pass are still only trusted once the state is saved after
/// them; see [`mtime_trusted`].
fn racy(mtime: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    mtime == 0
        || mtime >= now
        || (mtime.is_multiple_of(1_000_000_000) && now - mtime < 2_000_000_000)
}
