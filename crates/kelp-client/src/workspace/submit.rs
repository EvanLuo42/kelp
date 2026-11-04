use std::collections::{BTreeMap, HashMap};
use std::fs;

use kelp_core::{ClNumber, DepotPath, FileManifest, LocalClId};
use kelp_plugin::Plugin;
use kelp_proto::rpc::{FileInfo, SubmitAction, SubmitChange};
use kelp_proto::{Reply, Request};
use kelp_workspace::hunk::{Hunk, diff_hunks};
use kelp_workspace::{FileVersion, HaveEntry, HunkAnchor, OpenAction, Selection};
use tokio::task::JoinSet;

use super::{Workspace, version};
use crate::cache::write_file;
use crate::fsutil::{is_executable, remove_file, set_read_only};
use crate::index::DiskEntry;
use crate::plugins::{self, PluginChunker};
use crate::progress::Phase;
use crate::{Connection, ContentCache, Error, Result};

pub(crate) const PARALLEL_INGESTS: usize = 16;

/// The upload-ready form of a pending changelist.
struct Prepared {
    changes: Vec<SubmitChange>,
    manifests: Vec<FileManifest>,
    /// What each file's [`HaveEntry`] becomes after a submit.
    new_have: Vec<(DepotPath, Option<FileVersion>)>,
    /// Files submitted in full, as opposed to some of their hunks.
    whole: Vec<DepotPath>,
    /// The size and modification time of files read from disk, when read.
    observed: HashMap<DepotPath, DiskEntry>,
}

impl Workspace {
    /// Submits changelist `cl` and returns the server's changelist number.
    ///
    /// `description` overrides the pending changelist's description. Files
    /// with hunks in several changelists contribute only `cl`'s hunks and stay
    /// opened for the rest. If the changelist was shelved and the shelf is
    /// under review, the submit must match the review's latest version, and
    /// completes the review.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Remote`] if the server rejects the submit, for example
    /// because files changed since they were synced or an exclusive file is
    /// not locked, or an error if the changelist is empty, has no
    /// description, or has files that need a resolve.
    pub async fn submit(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        cl: LocalClId,
        description: Option<String>,
    ) -> Result<u64> {
        self.submit_fixing(conn, cache, cl, description, &[]).await
    }

    /// Like [`submit`](Self::submit), marking the changelist as fixing the
    /// issues `fixes`, such as `PROJ-123`. Issues the description mentions
    /// are linked as well.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`submit`](Self::submit), and
    /// [`Error::Remote`] if a key is malformed.
    pub async fn submit_fixing(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        cl: LocalClId,
        description: Option<String>,
        fixes: &[String],
    ) -> Result<u64> {
        let pending = self
            .state
            .pending
            .changelist(cl)
            .ok_or(kelp_workspace::Error::NoSuchChangelist(cl))?;
        let integration = pending
            .integration
            .clone()
            .map(|(source, through)| (source.to_string(), through));
        let review = pending.shelf.map(|c| c.0);
        let description = description.unwrap_or_else(|| pending.description.clone());
        if description.trim().is_empty() {
            return Err(Error::Invalid("a changelist needs a description".into()));
        }
        let prepared = self.prepare(conn, cache, cl).await?;
        cache.upload(conn, &prepared.manifests).await?;
        let submitted: std::collections::BTreeSet<&DepotPath> =
            prepared.new_have.iter().map(|(p, _)| p).collect();
        let moves: Vec<(DepotPath, DepotPath)> = self
            .state
            .moves
            .iter()
            .filter(|(to, from)| submitted.contains(to) && submitted.contains(from))
            .map(|(to, from)| (from.clone(), to.clone()))
            .collect();
        let request = Request::Submit {
            stream: self.stream.to_string(),
            workspace: self.config.name.clone(),
            description,
            changes: prepared.changes,
            integration,
            review,
            fixes: fixes.to_vec(),
            moves: moves
                .iter()
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .collect(),
        };
        let Reply::Submitted { cl: number, .. } = conn.call(&request).await? else {
            return Err(Error::UnexpectedReply);
        };

        for (path, file) in prepared.new_have {
            match file {
                Some(f) if prepared.whole.contains(&path) => {
                    let observed = prepared.observed.get(&path).copied();
                    self.record_have_observed(&path, f, observed)?
                }
                Some(f) => {
                    let local = self.view_map(&path)?;
                    let entry = HaveEntry {
                        local,
                        content: f.content,
                        size: f.size,
                        executable: f.executable,
                        mtime: 0,
                    };
                    self.state.have.insert(path.clone(), entry);
                }
                None => {
                    self.state.have.remove(&path);
                }
            }
            self.state.dirty.insert(path);
        }
        for path in &prepared.whole {
            if self.is_exclusive(path) {
                set_read_only(&self.local_path(path)?, true)?;
            }
        }
        for (_, to) in &moves {
            self.state.moves.remove(to);
        }
        self.state.pending.complete_submit(cl, &prepared.whole);
        self.save()?;
        Ok(number)
    }

    /// Shelves changelist `cl` on the server and returns the shelf number.
    ///
    /// Shelving again replaces the previous shelf of the same changelist.
    ///
    /// # Errors
    ///
    /// Returns an error if the changelist is empty or the server rejects it.
    pub async fn shelve(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        cl: LocalClId,
    ) -> Result<u64> {
        let pending = self
            .state
            .pending
            .changelist(cl)
            .ok_or(kelp_workspace::Error::NoSuchChangelist(cl))?;
        let (shelf, description) = (pending.shelf.map(|c| c.0), pending.description.clone());
        let prepared = self.prepare(conn, cache, cl).await?;
        cache.upload(conn, &prepared.manifests).await?;
        let request = Request::Shelve {
            shelf,
            stream: self.stream.to_string(),
            workspace: self.config.name.clone(),
            description,
            files: prepared.changes,
        };
        let Reply::Shelved { cl: number } = conn.call(&request).await? else {
            return Err(Error::UnexpectedReply);
        };
        self.state.pending.changelist_mut(cl)?.shelf = Some(ClNumber(number));
        self.save()?;
        Ok(number)
    }

    /// Opens the files of shelf `shelf` with their shelved content, in
    /// changelist `into` or a new changelist, and returns that changelist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the shelf belongs to another stream, a
    /// file is already opened, or the workspace does not have the content a
    /// shelved file was based on.
    pub async fn unshelve(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        shelf: u64,
        into: Option<LocalClId>,
    ) -> Result<(LocalClId, Vec<(DepotPath, OpenAction)>)> {
        if let Some(cl) = into
            && self.state.pending.changelist(cl).is_none()
        {
            return Err(kelp_workspace::Error::NoSuchChangelist(cl).into());
        }
        let Reply::Shelf(info) = conn.call(&Request::GetShelf { cl: shelf }).await? else {
            return Err(Error::UnexpectedReply);
        };
        if DepotPath::parse(&info.stream)? != self.stream {
            return Err(Error::Invalid(format!(
                "shelf {shelf} belongs to {}",
                info.stream
            )));
        }
        self.refresh_typemap(conn).await?;
        let mut plan = Vec::new();
        for file in &info.files {
            let path = self.stream.join(&file.path)?;
            self.view_map(&path)?;
            if self.state.pending.action(&path).is_some() {
                return Err(Error::Invalid(format!("{path} is already opened")));
            }
            let have = self.have_version(&path).map(|h| h.content);
            let (action, base, content) = match &file.action {
                SubmitAction::Add { file } => (OpenAction::Add, None, Some(*file)),
                SubmitAction::Edit { base, file } => (OpenAction::Edit, Some(*base), Some(*file)),
                SubmitAction::Delete { base } => (OpenAction::Delete, Some(*base), None),
            };
            if have != base {
                return Err(Error::Invalid(format!(
                    "sync {path} to the revision shelf {shelf} was based on first, \
                     or unshelve with --sync to have Kelp do it"
                )));
            }
            self.check_overwrite(&path, content.map(|c| c.content))?;
            plan.push((path, action, content));
        }
        let cl = match into {
            Some(cl) => cl,
            None => self
                .state
                .pending
                .create(format!("Unshelved from {shelf}: {}", info.description)),
        };
        let exclusive: Vec<DepotPath> = plan
            .iter()
            .map(|(p, _, _)| p)
            .filter(|p| self.is_exclusive(p))
            .cloned()
            .collect();
        self.lock(conn, &exclusive).await?;
        let contents: Vec<_> = plan
            .iter()
            .filter_map(|(_, _, c)| c.map(|c| c.content))
            .collect();
        cache.fetch(conn, &contents).await?;
        let tmp = self.tmp_dir();
        let mut opened = Vec::new();
        for (path, action, content) in plan {
            let abs = self.local_path(&path)?;
            match content {
                Some(f) => {
                    cache
                        .materialize(&f.content, f.executable, &abs, &tmp)
                        .await?
                }
                None => remove_file(&abs, &self.root)?,
            }
            let splittable = action == OpenAction::Edit && self.splittable(&path);
            self.state
                .pending
                .open(path.clone(), action, cl, splittable)?;
            opened.push((path, action));
        }
        self.save()?;
        Ok((cl, opened))
    }

    /// Brings the files of `shelf` to the revisions it was based on, so that
    /// [`unshelve`](Self::unshelve) accepts them, and returns the files it
    /// changed with the changelist each was synced to.
    ///
    /// A shelf records the content its files were based on, not a changelist.
    /// The changelist is found in each file's history. Only the files of the
    /// shelf are touched; the rest of the workspace stays where it is.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the shelf belongs to another stream, if
    /// no revision of a file has the content the shelf was based on, or if a
    /// file cannot be replaced because it is opened or has local changes.
    pub async fn sync_to_shelf_base(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        shelf: u64,
    ) -> Result<Vec<(DepotPath, u64)>> {
        let Reply::Shelf(info) = conn.call(&Request::GetShelf { cl: shelf }).await? else {
            return Err(Error::UnexpectedReply);
        };
        if DepotPath::parse(&info.stream)? != self.stream {
            return Err(Error::Invalid(format!(
                "shelf {shelf} belongs to {}",
                info.stream
            )));
        }
        let mut by_cl: BTreeMap<u64, Vec<DepotPath>> = BTreeMap::new();
        for file in &info.files {
            let base = match &file.action {
                SubmitAction::Add { .. } => continue,
                SubmitAction::Edit { base, .. } | SubmitAction::Delete { base } => *base,
            };
            let path = self.stream.join(&file.path)?;
            if self.have_version(&path).is_some_and(|h| h.content == base) {
                continue;
            }
            if self.state.pending.action(&path).is_some() {
                return Err(Error::Invalid(format!("{path} is already opened")));
            }
            let Reply::Filelog(revisions) = conn
                .call(&Request::Filelog {
                    path: path.to_string(),
                })
                .await?
            else {
                return Err(Error::UnexpectedReply);
            };
            // Newest first, so a content that came back is found at its
            // latest changelist.
            let revision = revisions
                .iter()
                .find(|r| r.content == Some(base))
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "no revision of {path} has the content shelf {shelf} was based on"
                    ))
                })?;
            by_cl.entry(revision.cl).or_default().push(path);
        }
        let mut synced = Vec::new();
        for (cl, paths) in by_cl {
            let report = self.sync_paths(conn, cache, Some(cl), &paths).await?;
            if let Some((path, reason)) = report.skipped.iter().find(|(p, _)| paths.contains(p)) {
                return Err(Error::Invalid(format!(
                    "could not sync {path} to changelist {cl} ({reason:?}); revert or remove \
                     your changes to it first"
                )));
            }
            synced.extend(paths.into_iter().map(|p| (p, cl)));
        }
        Ok(synced)
    }

    /// Returns the hunks of an opened text file and the changelist each belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotOpened`] if the file is not opened for edit, or
    /// [`Error::Invalid`] if it is not text.
    pub async fn hunks(
        &self,
        cache: &ContentCache,
        path: &DepotPath,
    ) -> Result<Vec<(Hunk, LocalClId)>> {
        if self.state.pending.action(path) != Some(OpenAction::Edit) {
            return Err(Error::NotOpened(path.clone()));
        }
        let (base, working) = self.texts(cache, path).await?;
        let owner = |a: &HunkAnchor| {
            self.state
                .pending
                .hunk_owner(path, a)
                .expect("file is opened")
        };
        Ok(diff_hunks(&base, &working)
            .into_iter()
            .map(|h| (owner(&h.anchor), h))
            .map(|(o, h)| (h, o))
            .collect())
    }

    async fn prepare(
        &self,
        conn: &Connection,
        cache: &ContentCache,
        cl: LocalClId,
    ) -> Result<Prepared> {
        let mut texts = HashMap::new();
        let mut anchors = HashMap::new();
        for path in self.state.pending.split_files() {
            let (base, working) = self.texts(cache, path).await?;
            anchors.insert(
                path.clone(),
                diff_hunks(&base, &working)
                    .into_iter()
                    .map(|h| h.anchor)
                    .collect(),
            );
            texts.insert(path.clone(), (base, working));
        }
        let files = self
            .state
            .pending
            .contents(cl, |p| anchors.get(p).cloned().unwrap_or_default());
        if files.is_empty() {
            return Err(Error::Invalid(format!("changelist {cl} has no files")));
        }
        enum Source {
            Disk(std::path::PathBuf),
            Text(Vec<u8>),
        }
        let mut plan = Vec::with_capacity(files.len());
        for (path, selection) in files {
            if self.state.resolves.contains_key(&path) {
                return Err(Error::Invalid(format!(
                    "{path} needs a resolve; run `kelp resolve`"
                )));
            }
            let action = self
                .state
                .pending
                .action(&path)
                .expect("listed files are opened");
            let abs = self.local_path(&path)?;
            let have = self.have_version(&path);
            let (source, executable) = match (action, &selection) {
                (OpenAction::Delete | OpenAction::MoveFrom, _) => (None, false),
                (_, Selection::Whole) => (Some(Source::Disk(abs.clone())), is_executable(&abs)?),
                (_, Selection::Hunks(_)) => {
                    let (base_text, working) = &texts[&path];
                    let composed = self
                        .state
                        .pending
                        .compose_text(cl, &path, base_text, working)?;
                    (
                        Some(Source::Text(composed.into_bytes())),
                        have.is_some_and(|h| h.executable),
                    )
                }
            };
            plan.push((path, selection, have, executable, source));
        }

        let mut chunkers: HashMap<String, Plugin> = HashMap::new();
        for (path, ..) in &plan {
            if let Some(reference) = self.file_type(path).chunker
                && reference.starts_with("plugin:")
                && !chunkers.contains_key(&reference)
            {
                let plugin = plugins::load(conn, cache, &reference).await?;
                chunkers.insert(reference, plugin);
            }
        }
        let mut ingests = JoinSet::new();
        let mut manifests: Vec<Option<FileManifest>> = vec![None; plan.len()];
        let mut observed: Vec<Option<DiskEntry>> = vec![None; plan.len()];
        let reads = plan.iter().filter(|(.., s)| s.is_some()).count();
        let mut meter = cache.meter(Phase::Read, reads as u64, None);
        for (i, (path, _, _, _, source)) in plan.iter_mut().enumerate() {
            let Some(source) = source.take() else {
                continue;
            };
            let chunker = self
                .file_type(path)
                .chunker
                .and_then(|r| chunkers.get(&r))
                .map(|p| PluginChunker {
                    plugin: p.clone(),
                    path: self.stream_relative(path),
                });
            if ingests.len() >= PARALLEL_INGESTS {
                let (j, manifest, seen): (_, FileManifest, _) = ingests
                    .join_next()
                    .await
                    .expect("ingests are pending")
                    .map_err(join_error)??;
                meter.add(1, manifest.size);
                manifests[j] = Some(manifest);
                observed[j] = seen;
            }
            let cache = cache.clone();
            ingests.spawn(async move {
                let (data, seen) = match source {
                    Source::Disk(abs) => {
                        let before = DiskEntry::of(&tokio::fs::metadata(&abs).await?);
                        let data = tokio::fs::read(&abs).await?;
                        let after = DiskEntry::of(&tokio::fs::metadata(&abs).await?);
                        if before != after || data.len() as u64 != after.size {
                            return Err(Error::Invalid(format!(
                                "{} changed while it was read; submit again",
                                abs.display()
                            )));
                        }
                        (data, Some(after))
                    }
                    Source::Text(bytes) => (bytes, None),
                };
                let manifest = match chunker {
                    Some(chunker) => cache.ingest_with(&data, &chunker).await?,
                    None => cache.ingest(&data).await?,
                };
                Ok::<_, Error>((i, manifest, seen))
            });
        }
        while let Some(done) = ingests.join_next().await {
            let (j, manifest, seen) = done.map_err(join_error)??;
            meter.add(1, manifest.size);
            manifests[j] = Some(manifest);
            observed[j] = seen;
        }

        let mut prepared = Prepared {
            changes: Vec::new(),
            manifests: Vec::new(),
            new_have: Vec::new(),
            whole: Vec::new(),
            observed: plan
                .iter()
                .zip(&observed)
                .filter_map(|((path, ..), seen)| seen.map(|s| (path.clone(), s)))
                .collect(),
        };
        for ((path, selection, have, executable, _), manifest) in plan.into_iter().zip(manifests) {
            let base = || {
                have.map(|h| h.content)
                    .ok_or_else(|| Error::NotTracked(path.clone()))
            };
            let action = match manifest {
                Some(manifest) => {
                    let file = FileInfo {
                        content: manifest.content,
                        size: manifest.size,
                        executable,
                    };
                    prepared.manifests.push(manifest);
                    prepared.new_have.push((path.clone(), Some(version(file))));
                    match have {
                        Some(_) => SubmitAction::Edit {
                            base: base()?,
                            file,
                        },
                        None => SubmitAction::Add { file },
                    }
                }
                None => {
                    prepared.new_have.push((path.clone(), None));
                    SubmitAction::Delete { base: base()? }
                }
            };
            prepared.changes.push(SubmitChange {
                path: self.stream_relative(&path),
                action,
            });
            if selection == Selection::Whole {
                prepared.whole.push(path);
            }
        }
        Ok(prepared)
    }

    /// Returns the base and working text of an opened file.
    async fn texts(&self, cache: &ContentCache, path: &DepotPath) -> Result<(String, String)> {
        let not_text = || Error::Invalid(format!("{path} is not a text file"));
        let base = match self.have_version(path) {
            Some(h) => String::from_utf8(cache.read(&h.content).await?).map_err(|_| not_text())?,
            None => String::new(),
        };
        let working =
            String::from_utf8(fs::read(self.local_path(path)?)?).map_err(|_| not_text())?;
        Ok((base, working))
    }

    /// Writes `data` to the workspace file of `path`.
    pub(super) fn write(&self, path: &DepotPath, data: &[u8], executable: bool) -> Result<()> {
        write_file(data, executable, &self.local_path(path)?, &self.tmp_dir())
    }
}

fn join_error(e: tokio::task::JoinError) -> Error {
    Error::Invalid(format!("ingest task failed: {e}"))
}
