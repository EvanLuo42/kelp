use std::fs;

use kelp_core::DepotPath;
use kelp_core::typemap::FileKind;
use kelp_plugin::MergeResult;
use kelp_workspace::{FileVersion, OpenAction, Resolve, ResolveKind};

use super::Workspace;
use crate::fsutil::{content_of, is_executable, remove_file, set_read_only};
use crate::plugins;
use crate::{Connection, ContentCache, Error, Result};

/// How [`Workspace::resolve`] reconciles two lines of changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResolveMode {
    /// Merges text files three ways; other files need an explicit choice.
    Auto,
    /// Keeps the workspace file.
    AcceptYours,
    /// Takes the incoming version.
    AcceptTheirs,
}

/// The result of resolving one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResolveOutcome {
    /// Both sides were merged without conflicts.
    Merged,
    /// The workspace file was kept.
    TookYours,
    /// The incoming version was taken.
    TookTheirs,
    /// Conflict markers were written into the file. Edit it, then resolve
    /// again with [`ResolveMode::AcceptYours`].
    Conflicted,
    /// The file is binary or one side deletes it, so it needs an explicit
    /// [`ResolveMode::AcceptYours`] or [`ResolveMode::AcceptTheirs`].
    NeedsChoice,
}

impl Workspace {
    /// Resolves pending resolves of `paths`, or of all files if empty.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if a path has no pending resolve, or an error
    /// if content cannot be fetched or written.
    pub async fn resolve(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        paths: &[DepotPath],
        mode: ResolveMode,
    ) -> Result<Vec<(DepotPath, ResolveOutcome)>> {
        let paths: Vec<DepotPath> = if paths.is_empty() {
            self.state.resolves.keys().cloned().collect()
        } else {
            paths.to_vec()
        };
        let mut contents = Vec::new();
        for path in &paths {
            let r = self
                .state
                .resolves
                .get(path)
                .ok_or_else(|| Error::Invalid(format!("{path} needs no resolve")))?;
            contents.extend(r.base.iter().chain(r.theirs.iter()).map(|v| v.content));
        }
        cache.fetch(conn, &contents).await?;
        if mode != ResolveMode::AcceptYours {
            self.auto_snapshot(cache, &paths, "resolve").await?;
        }

        let mut outcomes = Vec::new();
        for path in paths {
            let resolve = self.state.resolves[&path];
            let outcome = self.resolve_one(conn, cache, &path, resolve, mode).await?;
            if !matches!(
                outcome,
                ResolveOutcome::Conflicted | ResolveOutcome::NeedsChoice
            ) {
                self.finish(&path, resolve)?;
            }
            outcomes.push((path, outcome));
        }
        self.save()?;
        Ok(outcomes)
    }

    async fn resolve_one(
        &self,
        conn: &Connection,
        cache: &ContentCache,
        path: &DepotPath,
        resolve: Resolve,
        mode: ResolveMode,
    ) -> Result<ResolveOutcome> {
        let deleted = self.state.pending.action(path) == Some(OpenAction::Delete);
        match mode {
            ResolveMode::AcceptYours => return Ok(ResolveOutcome::TookYours),
            ResolveMode::AcceptTheirs => {
                match resolve.theirs {
                    Some(t) => self.write(path, &cache.read(&t.content).await?, t.executable)?,
                    None => remove_file(&self.local_path(path)?, &self.root)?,
                }
                return Ok(ResolveOutcome::TookTheirs);
            }
            ResolveMode::Auto => {}
        }
        if let Some(reference) = self.file_type(path).merge
            && reference.starts_with("plugin:")
        {
            return self
                .merge_with_plugin(conn, cache, path, resolve, &reference, deleted)
                .await;
        }
        let Some(theirs) = resolve
            .theirs
            .filter(|_| !deleted && self.file_type(path).kind != FileKind::Binary)
        else {
            return Ok(ResolveOutcome::NeedsChoice);
        };
        let base = match resolve.base {
            Some(b) => cache.read(&b.content).await?,
            None => Vec::new(),
        };
        let abs = self.local_path(path)?;
        let texts = (
            String::from_utf8(base),
            String::from_utf8(fs::read(&abs)?),
            String::from_utf8(cache.read(&theirs.content).await?),
        );
        let (Ok(base), Ok(yours), Ok(theirs_text)) = texts else {
            return Ok(ResolveOutcome::NeedsChoice);
        };
        let executable = is_executable(&abs)?;
        match diffy::merge(&base, &yours, &theirs_text) {
            Ok(merged) => {
                self.write(path, merged.as_bytes(), executable)?;
                Ok(ResolveOutcome::Merged)
            }
            Err(conflicted) => {
                self.write(path, conflicted.as_bytes(), executable)?;
                Ok(ResolveOutcome::Conflicted)
            }
        }
    }

    /// Merges a file with the merger plugin `reference`. A conflict leaves the
    /// file for the user to choose.
    async fn merge_with_plugin(
        &self,
        conn: &Connection,
        cache: &ContentCache,
        path: &DepotPath,
        resolve: Resolve,
        reference: &str,
        deleted: bool,
    ) -> Result<ResolveOutcome> {
        let Some(theirs) = resolve.theirs.filter(|_| !deleted) else {
            return Ok(ResolveOutcome::NeedsChoice);
        };
        let plugin = plugins::load(conn, cache, reference).await?;
        let rel = self.stream_relative(path);
        let version = |content| kelp_plugin::FileVersion {
            path: rel.clone(),
            content,
        };
        let base = match resolve.base {
            Some(b) => cache.read(&b.content).await?,
            None => Vec::new(),
        };
        let abs = self.local_path(path)?;
        let (base, ours, theirs_v) = (
            version(base),
            version(fs::read(&abs)?),
            version(cache.read(&theirs.content).await?),
        );
        match plugin.merge(plugins::host()?, &base, &ours, &theirs_v)? {
            MergeResult::Merged(data) => {
                self.write(path, &data, is_executable(&abs)?)?;
                Ok(ResolveOutcome::Merged)
            }
            MergeResult::Conflict(_) => Ok(ResolveOutcome::NeedsChoice),
        }
    }

    /// Updates the base and opened action of a resolved file.
    fn finish(&mut self, path: &DepotPath, resolve: Resolve) -> Result<()> {
        self.state.resolves.remove(path);
        if resolve.kind == ResolveKind::Sync {
            match resolve.theirs {
                Some(t) => self.record_have_keeping_mtime(path, t)?,
                None => {
                    self.state.have.remove(path);
                }
            }
        }
        let abs = self.local_path(path)?;
        let exists = abs.exists();
        let tracked = self.state.have.contains_key(path);
        let cl = self
            .state
            .pending
            .owner(path)
            .expect("resolved files are opened");
        let action = match (exists, tracked) {
            (true, true) => OpenAction::Edit,
            (true, false) => OpenAction::Add,
            (false, true) => OpenAction::Delete,
            (false, false) => {
                self.state.pending.revert(path)?;
                return Ok(());
            }
        };
        if self.state.pending.action(path) != Some(action) {
            let splittable = action == OpenAction::Edit && self.splittable(path);
            self.state
                .pending
                .open(path.clone(), action, cl, splittable)?;
        }
        if exists
            && action == OpenAction::Edit
            && self
                .state
                .have
                .get(path)
                .is_some_and(|h| content_of(&abs).is_ok_and(|c| c == h.content))
        {
            set_read_only(&abs, false)?;
        }
        Ok(())
    }

    /// Records `file` as the base of `path` without claiming the disk matches it.
    fn record_have_keeping_mtime(&mut self, path: &DepotPath, file: FileVersion) -> Result<()> {
        let local = self.view_map(path)?;
        let entry = kelp_workspace::HaveEntry {
            local,
            content: file.content,
            size: file.size,
            executable: file.executable,
            mtime: 0,
        };
        self.state.have.insert(path.clone(), entry);
        Ok(())
    }
}
