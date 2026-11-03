use std::collections::BTreeMap;

use kelp_core::{ContentId, DepotPath};
use kelp_workspace::{Resolve, ResolveKind};

use super::{ArtifactUpdate, ProjectedFile, Workspace, version};
use crate::fsutil::{content_of, file_matches, remove_file, set_read_only};
use crate::progress::Phase;
use crate::{Connection, ContentCache, Error, Result};

/// Why sync left a file untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SkipReason {
    /// The file is opened; a resolve has been scheduled.
    Opened,
    /// The file was changed on disk without being opened.
    LocallyModified,
    /// A file not known to the workspace is in the way.
    Untracked,
}

/// The outcome of [`Workspace::sync`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncReport {
    /// The changelist the workspace is now synced to.
    pub cl: u64,
    pub updated: Vec<DepotPath>,
    pub deleted: Vec<DepotPath>,
    pub skipped: Vec<(DepotPath, SkipReason)>,
    /// Artifact directories that now hold a different artifact.
    pub artifacts: Vec<ArtifactUpdate>,
}

impl Workspace {
    /// Brings the workspace to changelist `at` of its stream, or to the head.
    ///
    /// Opened files are left alone and get a resolve scheduled. Files modified
    /// on disk without being opened, and untracked files in the way, are left
    /// alone and reconsidered on the next sync. Files of exclusive types are
    /// written read-only.
    ///
    /// The artifacts configured in [`Config::artifacts`] are then replaced
    /// with the ones built from the synced changelist.
    ///
    /// [`Config::artifacts`]: kelp_workspace::Config::artifacts
    ///
    /// # Errors
    ///
    /// Returns an error if the server cannot be reached or a file cannot be
    /// written.
    pub async fn sync(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        at: Option<u64>,
    ) -> Result<SyncReport> {
        self.sync_paths(conn, cache, at, &[]).await
    }

    /// Like [`sync`](Self::sync), but only updates files at or below
    /// `filter`, or every file if it is empty.
    ///
    /// Files elsewhere that differ are remembered and brought up to date
    /// by a later sync that covers them.
    ///
    /// # Errors
    ///
    /// As for [`sync`](Self::sync).
    pub async fn sync_paths(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        at: Option<u64>,
        filter: &[DepotPath],
    ) -> Result<SyncReport> {
        self.refresh_typemap(conn).await?;
        let stream = self.stream.to_string();
        let head = conn.resolve_stream(&stream, at).await?;
        let dirty: Vec<DepotPath> = self.state.dirty.iter().cloned().collect();
        let paths = dirty.iter().map(|p| self.stream_relative(p)).collect();
        let (changes, lookups) = conn
            .diff_tree(&stream, self.state.have_root, head.root, paths)
            .await?;
        let mut targets = BTreeMap::new();
        for delta in changes {
            targets.insert(self.stream.join(&delta.path)?, delta.new);
        }
        targets.extend(dirty.into_iter().zip(lookups));
        let mut deferred = Vec::new();
        if !filter.is_empty() {
            targets.retain(|path, _| {
                let wanted = filter.iter().any(|f| path.starts_with(f));
                if !wanted {
                    deferred.push(path.clone());
                }
                wanted
            });
        }

        let mut report = SyncReport {
            cl: head.cl,
            ..SyncReport::default()
        };
        let mut plan = Vec::new();
        let mut scan = cache.meter(Phase::Scan, targets.len() as u64, None);
        for (path, target) in targets {
            scan.add(1, 0);
            let Some(local) = self.view.map(&path) else {
                continue;
            };
            let have = self.have_version(&path);
            if have.map(|h| h.content) == target.map(|t| t.content) {
                continue;
            }
            if self.state.pending.action(&path).is_some() {
                if self
                    .state
                    .resolves
                    .get(&path)
                    .is_some_and(|r| r.kind == ResolveKind::Merge)
                {
                    // Replacing it would drop the merged-in change; it has to
                    // be resolved first.
                    report.skipped.push((path, SkipReason::Opened));
                    continue;
                }
                let resolve = Resolve {
                    kind: ResolveKind::Sync,
                    base: have,
                    theirs: target.map(version),
                };
                self.state.resolves.insert(path.clone(), resolve);
                report.skipped.push((path, SkipReason::Opened));
                continue;
            }
            let abs = self.abs(&local);
            let entry = self.state.have.get(&path);
            match entry {
                Some(h) if abs.exists() && !file_matches(&abs, h)? => match target {
                    // An interrupted sync may have written it already.
                    Some(t) if content_of(&abs).is_ok_and(|c| c == t.content) => {
                        self.record_have(&path, version(t))?;
                        report.updated.push(path);
                    }
                    _ => report.skipped.push((path, SkipReason::LocallyModified)),
                },
                // A directory in the way may be emptied by this sync.
                None if abs.exists() && !abs.is_dir() => match target {
                    Some(t) if content_of(&abs).is_ok_and(|c| c == t.content) => {
                        self.record_have(&path, version(t))?;
                        report.updated.push(path);
                    }
                    _ => report.skipped.push((path, SkipReason::Untracked)),
                },
                _ => plan.push((path, target)),
            }
        }

        // Deleting first lets a directory become a file, and a file a
        // directory, in one sync.
        plan.sort_by_key(|(_, target)| target.is_some());
        if self.config.projected {
            let projector = self.projector.clone().ok_or_else(|| {
                Error::Invalid("this projected workspace is not mounted; is kelpd running?".into())
            })?;
            for (path, target) in plan {
                let local = self.view_map(&path)?;
                let file = target.map(|t| ProjectedFile {
                    version: version(t),
                    read_only: self.is_exclusive(&path),
                });
                if !projector.project(&local, file)? {
                    report.skipped.push((path, SkipReason::LocallyModified));
                    continue;
                }
                match target {
                    Some(t) => {
                        self.record_have(&path, version(t))?;
                        report.updated.push(path);
                    }
                    None => {
                        self.state.have.remove(&path);
                        report.deleted.push(path);
                    }
                }
            }
        } else {
            let contents: Vec<ContentId> = plan
                .iter()
                .filter_map(|(_, t)| t.map(|t| t.content))
                .collect();
            cache.fetch(conn, &contents).await?;
            let tmp = self.tmp_dir();
            let bytes = plan.iter().filter_map(|(_, t)| t.map(|t| t.size)).sum();
            let mut meter = cache.meter(Phase::Write, plan.len() as u64, Some(bytes));
            for (path, target) in plan {
                let size = target.map_or(0, |t| t.size);
                let abs = self.local_path(&path)?;
                if target.is_some() && abs.is_dir() {
                    meter.add(1, size);
                    report.skipped.push((path, SkipReason::Untracked));
                    continue;
                }
                // Downloading can take long; check again just before acting.
                if self
                    .check_overwrite(&path, target.map(|t| t.content))
                    .is_err()
                {
                    meter.add(1, size);
                    report.skipped.push((path, SkipReason::LocallyModified));
                    continue;
                }
                match target {
                    Some(t) => {
                        // The bytes are counted as they are written.
                        cache
                            .materialize_metered(
                                &t.content,
                                t.executable,
                                &abs,
                                &tmp,
                                Some(&mut meter),
                            )
                            .await?;
                        meter.add(1, 0);
                        set_read_only(&abs, self.is_exclusive(&path))?;
                        self.record_have(&path, version(t))?;
                        report.updated.push(path);
                    }
                    None => {
                        remove_file(&abs, &self.root)?;
                        meter.add(1, 0);
                        self.state.have.remove(&path);
                        report.deleted.push(path);
                    }
                }
            }
        }
        self.state.have_root = Some(head.root);
        self.state.have_cl = head.cl;
        self.state.dirty = report
            .skipped
            .iter()
            .map(|(p, _)| p.clone())
            .chain(deferred)
            .collect();
        self.save()?;
        report.artifacts = self.sync_artifacts(conn, cache, head.cl).await?;
        self.save()?;
        Ok(report)
    }
}
