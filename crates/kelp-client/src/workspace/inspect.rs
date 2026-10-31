use std::collections::HashMap;

use kelp_core::{DepotPath, LocalClId};
use kelp_proto::{Reply, Request};
use kelp_workspace::OpenAction;
use serde::{Deserialize, Serialize};

use super::Workspace;
use crate::fsutil::file_matches;
use crate::{Connection, Error, Result};

/// Everything an editor needs to show about one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReport {
    pub path: DepotPath,
    /// Whether the workspace has the file from the server.
    pub tracked: bool,
    /// Whether the file exists on disk.
    pub exists: bool,
    /// How the file is opened, and in which changelist.
    pub opened: Option<(OpenAction, LocalClId)>,
    /// Whether the file was changed on disk without being opened.
    pub modified: bool,
    pub needs_resolve: bool,
    /// Whether the typemap makes the file exclusive.
    pub exclusive: bool,
    /// Who holds the file's lock, as `(user, workspace)`.
    pub locked_by: Option<(String, String)>,
    /// Whether this workspace holds the lock.
    pub locked_here: bool,
    /// Whether the stream has a newer revision than the workspace.
    pub out_of_date: bool,
}

impl Workspace {
    /// Reports the state of `paths`, asking the server about locks and newer
    /// revisions.
    ///
    /// # Errors
    ///
    /// Returns an error if the server cannot be reached.
    pub async fn inspect(
        &mut self,
        conn: &Connection,
        paths: &[DepotPath],
    ) -> Result<Vec<FileReport>> {
        self.refresh_typemap(conn).await?;
        let stream = self.stream.to_string();
        let Reply::Locks(locks) = conn
            .call(&Request::ListLocks {
                stream: stream.clone(),
            })
            .await?
        else {
            return Err(Error::UnexpectedReply);
        };
        let locks: HashMap<String, (String, String)> = locks
            .into_iter()
            .map(|l| (kelp_core::path::fold(&l.path), (l.user, l.workspace)))
            .collect();
        let head = conn.resolve_stream(&stream, None).await?;
        let rels: Vec<String> = paths.iter().map(|p| self.stream_relative(p)).collect();
        let (_, lookups) = conn
            .diff_tree(&stream, Some(head.root), head.root, rels.clone())
            .await?;
        let me = conn.welcome().user.clone().unwrap_or_default();

        let mut reports = Vec::with_capacity(paths.len());
        for ((path, rel), head_file) in paths.iter().zip(&rels).zip(lookups) {
            let have = self.state.have.get(path);
            let abs = self.local_path(path)?;
            let exists = abs.exists();
            let opened = self
                .state
                .pending
                .action(path)
                .zip(self.state.pending.owner(path));
            let modified = opened.is_none()
                && match have {
                    Some(h) => !file_matches(&abs, h)?,
                    None => exists,
                };
            let locked_by = locks.get(&kelp_core::path::fold(rel)).cloned();
            let locked_here = locked_by
                .as_ref()
                .is_some_and(|(u, w)| *u == me && *w == self.config.name);
            reports.push(FileReport {
                path: path.clone(),
                tracked: have.is_some(),
                exists,
                opened,
                modified,
                needs_resolve: self.state.resolves.contains_key(path),
                exclusive: self.is_exclusive(path),
                locked_by,
                locked_here,
                out_of_date: have.map(|h| h.content) != head_file.map(|f| f.content),
            });
        }
        Ok(reports)
    }
}
