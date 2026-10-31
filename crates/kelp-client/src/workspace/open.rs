use kelp_core::{DepotPath, LocalClId};
use kelp_proto::Request;
use kelp_workspace::OpenAction;

use super::Workspace;
use crate::fsutil::{remove_file, set_read_only};
use crate::{Connection, ContentCache, Error, Result};

impl Workspace {
    /// Opens every unopened change on disk in the default changelist.
    ///
    /// Exclusive files are locked first; if any lock is taken by someone else
    /// nothing is opened.
    ///
    /// # Errors
    ///
    /// Returns an error if the workspace cannot be read, a lock cannot be
    /// taken, or the state cannot be saved.
    pub async fn reconcile(&mut self, conn: &Connection) -> Result<Vec<(DepotPath, OpenAction)>> {
        self.refresh_typemap(conn).await?;
        let status = self.status()?;
        let opened: Vec<(DepotPath, OpenAction)> = [
            (status.modified, OpenAction::Edit),
            (status.added, OpenAction::Add),
            (status.deleted, OpenAction::Delete),
        ]
        .into_iter()
        .flat_map(|(paths, action)| paths.into_iter().map(move |p| (p, action)))
        .collect();
        let exclusive: Vec<DepotPath> = opened
            .iter()
            .map(|(p, _)| p)
            .filter(|p| self.is_exclusive(p))
            .cloned()
            .collect();
        self.lock(conn, &exclusive).await?;
        for (path, action) in &opened {
            let splittable = *action == OpenAction::Edit && self.splittable(path);
            self.state
                .pending
                .open(path.clone(), *action, LocalClId::DEFAULT, splittable)?;
        }
        self.save()?;
        Ok(opened)
    }

    /// Moves the file `from` to `to` on disk and opens the move in
    /// changelist `cl`. Submitting it deletes `from` and adds `to`, whose
    /// history then continues that of `from`. An opened edit of `from`
    /// moves along.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotTracked`] if `from` is not in the workspace,
    /// [`Error::AlreadyTracked`] if `to` is, [`Error::Invalid`] if `from` is
    /// opened other than for edit or something is at `to` on disk, or an
    /// error if a lock cannot be taken or the file cannot be moved.
    pub async fn move_file(
        &mut self,
        conn: &Connection,
        from: &DepotPath,
        to: &DepotPath,
        cl: LocalClId,
    ) -> Result<()> {
        if self.state.pending.changelist(cl).is_none() {
            return Err(kelp_workspace::Error::NoSuchChangelist(cl).into());
        }
        if !self.state.have.contains_key(from) {
            return Err(Error::NotTracked(from.clone()));
        }
        if self.state.have.contains_key(to) || self.state.pending.action(to).is_some() {
            return Err(Error::AlreadyTracked(to.clone()));
        }
        match self.state.pending.action(from) {
            None | Some(OpenAction::Edit) => {}
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "{from} is opened for {other:?}; revert it first"
                )));
            }
        }
        let (abs_from, abs_to) = (self.local_path(from)?, self.local_path(to)?);
        if abs_to.exists() {
            return Err(Error::Invalid(format!(
                "{} is in the way",
                abs_to.display()
            )));
        }
        self.refresh_typemap(conn).await?;
        let exclusive: Vec<DepotPath> = [from, to]
            .into_iter()
            .filter(|p| self.is_exclusive(p))
            .cloned()
            .collect();
        self.lock(conn, &exclusive).await?;
        if let Some(parent) = abs_to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        set_read_only(&abs_from, false)?;
        std::fs::rename(&abs_from, &abs_to)?;
        remove_file(&abs_from, &self.root)?;
        if self.state.pending.action(from).is_some() {
            self.state.pending.revert(from)?;
        }
        self.state
            .pending
            .open(from.clone(), OpenAction::MoveFrom, cl, false)?;
        self.state
            .pending
            .open(to.clone(), OpenAction::MoveTo, cl, false)?;
        self.state.moves.insert(to.clone(), from.clone());
        self.save()
    }

    /// Opens files for `action` in changelist `cl`.
    ///
    /// Files of exclusive types are locked on the server and made writable.
    /// Opening for [`OpenAction::Delete`] also removes the file from disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AlreadyTracked`] when adding a tracked file,
    /// [`Error::NotTracked`] when editing or deleting an untracked one,
    /// [`Error::Remote`] if a lock is held by someone else, or an error if `cl`
    /// does not exist.
    pub async fn open_files(
        &mut self,
        conn: &Connection,
        paths: &[DepotPath],
        action: OpenAction,
        cl: LocalClId,
    ) -> Result<()> {
        if self.state.pending.changelist(cl).is_none() {
            return Err(kelp_workspace::Error::NoSuchChangelist(cl).into());
        }
        for path in paths {
            let tracked = self.state.have.contains_key(path);
            match action {
                OpenAction::Add if tracked => return Err(Error::AlreadyTracked(path.clone())),
                OpenAction::Edit | OpenAction::Delete if !tracked => {
                    return Err(Error::NotTracked(path.clone()));
                }
                OpenAction::Delete if self.state.pending.action(path).is_some() => {
                    return Err(Error::Invalid(format!(
                        "{path} is already opened; revert it before opening it for delete"
                    )));
                }
                OpenAction::MoveFrom | OpenAction::MoveTo => {
                    return Err(Error::Invalid("moves are not supported yet".into()));
                }
                _ => {}
            }
        }
        self.refresh_typemap(conn).await?;
        let exclusive: Vec<DepotPath> = paths
            .iter()
            .filter(|p| self.is_exclusive(p))
            .cloned()
            .collect();
        self.lock(conn, &exclusive).await?;
        for path in paths {
            let splittable = action == OpenAction::Edit && self.splittable(path);
            self.state
                .pending
                .open(path.clone(), action, cl, splittable)?;
            let abs = self.local_path(path)?;
            match action {
                OpenAction::Delete => remove_file(&abs, &self.root)?,
                _ => set_read_only(&abs, false)?,
            }
        }
        self.save()
    }

    /// Reverts opened files, restoring edited and deleted files from the
    /// local cache and releasing their locks. Added files stay on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotOpened`] if a file is not opened, or
    /// [`Error::NotCached`] if its content is no longer cached.
    pub async fn revert(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        paths: &[DepotPath],
    ) -> Result<()> {
        if let Some(p) = paths
            .iter()
            .find(|p| self.state.pending.action(p).is_none())
        {
            return Err(Error::NotOpened(p.clone()));
        }
        // The two halves of a move are reverted together.
        let mut paths = paths.to_vec();
        for (to, from) in &self.state.moves {
            if paths.contains(to) != paths.contains(from) {
                paths.push(if paths.contains(to) {
                    from.clone()
                } else {
                    to.clone()
                });
            }
        }
        let paths = paths.as_slice();
        self.auto_snapshot(cache, paths, "revert").await?;
        let tmp = self.tmp_dir();
        for path in paths {
            let action = self.state.pending.action(path).expect("checked above");
            if action == OpenAction::MoveTo {
                remove_file(&self.local_path(path)?, &self.root)?;
                self.state.moves.remove(path);
            }
            if let (true, Some(have)) = (action != OpenAction::Add, self.have_version(path)) {
                let abs = self.local_path(path)?;
                cache
                    .materialize(&have.content, have.executable, &abs, &tmp)
                    .await?;
                set_read_only(&abs, self.is_exclusive(path))?;
                self.record_have(path, have)?;
            }
            if self.state.resolves.remove(path).is_some() {
                self.state.dirty.insert(path.clone());
            }
            self.state.pending.revert(path)?;
        }
        let exclusive: Vec<DepotPath> = paths
            .iter()
            .filter(|p| self.is_exclusive(p))
            .cloned()
            .collect();
        self.unlock(conn, &exclusive, false).await?;
        self.save()
    }

    /// Locks `paths` for this workspace, all or none.
    pub(super) async fn lock(&self, conn: &Connection, paths: &[DepotPath]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let request = Request::AcquireLocks {
            stream: self.stream.to_string(),
            workspace: self.config.name.clone(),
            paths: paths.iter().map(|p| self.stream_relative(p)).collect(),
        };
        conn.call(&request).await.map(drop)
    }

    /// Releases this workspace's locks on `paths`, or anyone's with `force`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Remote`] if `force` is set and the user is not an
    /// administrator.
    pub async fn unlock(&self, conn: &Connection, paths: &[DepotPath], force: bool) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let request = Request::ReleaseLocks {
            stream: self.stream.to_string(),
            workspace: self.config.name.clone(),
            paths: paths.iter().map(|p| self.stream_relative(p)).collect(),
            force,
        };
        conn.call(&request).await.map(drop)
    }
}
