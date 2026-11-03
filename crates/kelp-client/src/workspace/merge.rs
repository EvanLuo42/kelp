use kelp_core::{ContentId, DepotPath};
use kelp_proto::{Reply, Request};
use kelp_workspace::{OpenAction, Resolve, ResolveKind};

use super::{Workspace, version};
use crate::fsutil::remove_file;
use crate::progress::Phase;
use crate::{Connection, ContentCache, Error, Result};

/// The outcome of [`Workspace::merge`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeReport {
    /// The changelist holding the merge, if anything needed merging.
    pub changelist: Option<kelp_core::LocalClId>,
    /// The last source changelist merged.
    pub through: u64,
    /// Files opened by the merge.
    pub opened: Vec<(DepotPath, OpenAction)>,
    /// Opened files changed on both sides; they need a resolve.
    pub conflicts: Vec<DepotPath>,
}

impl Workspace {
    /// Merges the changes `source` (default: the stream's parent) made since
    /// the last merge into a new pending changelist.
    ///
    /// Files changed only in `source` are updated on disk. Files changed on
    /// both sides get a resolve scheduled. Submitting the changelist records
    /// the merge, so the next merge starts from here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the workspace is not synced to the head of
    /// its stream, has files that need a resolve, or a file to merge is
    /// already opened; or an error if the server rejects the merge.
    pub async fn merge(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        source: Option<DepotPath>,
    ) -> Result<MergeReport> {
        let head = conn.resolve_stream(self.stream.as_str(), None).await?;
        let source = match source {
            Some(s) => s,
            None => DepotPath::parse(
                head.parent
                    .as_deref()
                    .ok_or_else(|| Error::Invalid(format!("{} has no parent", self.stream)))?,
            )?,
        };
        if self.state.have_root != Some(head.root)
            || !self.state.dirty.is_empty()
            || !self.state.resolves.is_empty()
        {
            return Err(Error::Invalid(
                "sync to the head of the stream and resolve all files before merging".into(),
            ));
        }
        let request = Request::GetIntegration {
            target: self.stream.to_string(),
            source: source.to_string(),
        };
        let Reply::Integration(integration) = conn.call(&request).await? else {
            return Err(Error::UnexpectedReply);
        };
        let report = MergeReport {
            through: integration.source_cl,
            ..MergeReport::default()
        };
        if integration.base_root == integration.source_root {
            return Ok(report);
        }
        let (changes, _) = conn
            .diff_tree(
                source.as_str(),
                Some(integration.base_root),
                integration.source_root,
                Vec::new(),
            )
            .await?;
        let description = format!(
            "Merge {source}@{} into {}",
            integration.source_cl, self.stream
        );
        let integration = Some((source, integration.source_cl));
        self.open_changes(conn, cache, changes, description, integration, report)
            .await
    }

    /// Opens the changes of changelist `cl` reversed in a new pending
    /// changelist.
    ///
    /// Files unchanged since `cl` are put back as they were before it.
    /// Files changed since get a resolve scheduled.
    ///
    /// # Errors
    ///
    /// As for [`merge`](Self::merge), and [`Error::Invalid`] if `cl` did not
    /// change the workspace's stream.
    pub async fn undo(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        cl: u64,
    ) -> Result<MergeReport> {
        let stream = self.stream.to_string();
        self.require_synced(conn).await?;
        let before = conn
            .resolve_stream(&stream, Some(cl.saturating_sub(1)))
            .await?;
        let after = conn.resolve_stream(&stream, Some(cl)).await?;
        if after.cl != cl {
            return Err(Error::Invalid(format!(
                "changelist {cl} did not change {stream}"
            )));
        }
        let (changes, _) = conn
            .diff_tree(&stream, Some(after.root), before.root, Vec::new())
            .await?;
        let report = MergeReport {
            through: cl,
            ..MergeReport::default()
        };
        let description = format!("Undo changelist {cl}");
        self.open_changes(conn, cache, changes, description, None, report)
            .await
    }

    /// Opens the changes changelist `cl` made to `source` (default: the
    /// stream it was submitted to) in a new pending changelist, without
    /// recording a merge.
    ///
    /// # Errors
    ///
    /// As for [`undo`](Self::undo), with `cl` checked against `source`.
    pub async fn cherry_pick(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        source: Option<DepotPath>,
        cl: u64,
    ) -> Result<MergeReport> {
        let source = match source {
            Some(s) => s,
            None => {
                let Reply::Describe(change) = conn.call(&Request::Describe { cl }).await? else {
                    return Err(Error::UnexpectedReply);
                };
                DepotPath::parse(&change.summary.stream)?
            }
        };
        self.require_synced(conn).await?;
        let before = conn
            .resolve_stream(source.as_str(), Some(cl.saturating_sub(1)))
            .await?;
        let after = conn.resolve_stream(source.as_str(), Some(cl)).await?;
        if after.cl != cl {
            return Err(Error::Invalid(format!(
                "changelist {cl} did not change {source}"
            )));
        }
        let (changes, _) = conn
            .diff_tree(source.as_str(), Some(before.root), after.root, Vec::new())
            .await?;
        let report = MergeReport {
            through: cl,
            ..MergeReport::default()
        };
        let description = format!("Pick changelist {cl} from {source}");
        self.open_changes(conn, cache, changes, description, None, report)
            .await
    }

    async fn require_synced(&self, conn: &Connection) -> Result<()> {
        let head = conn.resolve_stream(self.stream.as_str(), None).await?;
        if self.state.have_root != Some(head.root)
            || !self.state.dirty.is_empty()
            || !self.state.resolves.is_empty()
        {
            return Err(Error::Invalid(
                "sync to the head of the stream and resolve all files first".into(),
            ));
        }
        Ok(())
    }

    /// Opens `changes`, given relative to a stream root, in a new pending
    /// changelist: files the workspace has as their `old` side are updated
    /// to the `new` side, other differing files get a merge resolve.
    async fn open_changes(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        changes: Vec<kelp_proto::rpc::FileDelta>,
        description: String,
        integration: Option<(DepotPath, u64)>,
        mut report: MergeReport,
    ) -> Result<MergeReport> {
        let mut copies = Vec::new();
        let mut conflicts = Vec::new();
        for delta in changes {
            let path = self.stream.join(&delta.path)?;
            if self.view.map(&path).is_none() {
                continue;
            }
            let have = self.have_version(&path);
            let (base, theirs) = (delta.old.map(version), delta.new.map(version));
            if have.map(|h| h.content) == theirs.map(|t| t.content) {
                continue;
            }
            if self.state.pending.action(&path).is_some() {
                return Err(Error::Invalid(format!(
                    "{path} is opened; submit or revert it first"
                )));
            }
            if have.map(|h| h.content) == base.map(|b| b.content) {
                copies.push((path, theirs));
            } else {
                conflicts.push((
                    path,
                    Resolve {
                        kind: ResolveKind::Merge,
                        base,
                        theirs,
                    },
                ));
            }
        }
        if copies.is_empty() && conflicts.is_empty() {
            return Ok(report);
        }
        for (path, theirs) in &copies {
            self.check_overwrite(path, theirs.map(|t| t.content))?;
        }

        let cl = self.state.pending.create(description);
        self.state.pending.changelist_mut(cl)?.integration = integration;
        let touched: Vec<DepotPath> = copies
            .iter()
            .map(|(p, _)| p)
            .chain(conflicts.iter().map(|(p, _)| p))
            .cloned()
            .collect();
        let exclusive: Vec<DepotPath> = touched
            .iter()
            .filter(|p| self.is_exclusive(p))
            .cloned()
            .collect();
        if let Err(e) = self.lock(conn, &exclusive).await {
            self.state.pending.delete(cl)?;
            return Err(e);
        }
        let contents: Vec<ContentId> = copies
            .iter()
            .filter_map(|(_, t)| t.map(|t| t.content))
            .collect();
        cache.fetch(conn, &contents).await?;
        let tmp = self.tmp_dir();
        let bytes = copies.iter().filter_map(|(_, t)| t.map(|t| t.size)).sum();
        let mut meter = cache.meter(Phase::Write, copies.len() as u64, Some(bytes));
        for (path, theirs) in copies {
            meter.add(1, theirs.map_or(0, |t| t.size));
            let abs = self.local_path(&path)?;
            let tracked = self.state.have.contains_key(&path);
            let action = match theirs {
                Some(t) => {
                    cache
                        .materialize(&t.content, t.executable, &abs, &tmp)
                        .await?;
                    if tracked {
                        OpenAction::Edit
                    } else {
                        OpenAction::Add
                    }
                }
                None => {
                    remove_file(&abs, &self.root)?;
                    OpenAction::Delete
                }
            };
            self.state.pending.open(path.clone(), action, cl, false)?;
            report.opened.push((path, action));
        }
        for (path, resolve) in conflicts {
            let action = if self.state.have.contains_key(&path) {
                OpenAction::Edit
            } else {
                OpenAction::Add
            };
            let splittable = action == OpenAction::Edit && self.splittable(&path);
            self.state
                .pending
                .open(path.clone(), action, cl, splittable)?;
            self.state.resolves.insert(path.clone(), resolve);
            report.opened.push((path.clone(), action));
            report.conflicts.push(path);
        }
        report.changelist = Some(cl);
        self.save()?;
        Ok(report)
    }
}
