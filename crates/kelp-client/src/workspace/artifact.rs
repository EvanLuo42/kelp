use std::collections::BTreeMap;
use std::fs;

use kelp_core::DepotPath;
use kelp_workspace::{ArtifactHave, ArtifactMount};

use super::{Workspace, version};
use crate::artifact::{self, ArtifactQuery};
use crate::fsutil::{remove_file, set_read_only};
use crate::progress::Phase;
use crate::{Connection, ContentCache, Error, Result};

/// What sync did to one of the workspace's artifact directories.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactUpdate {
    /// The directory, as in [`ArtifactMount::to`].
    pub to: String,
    pub name: String,
    /// The artifact now in the directory and the changelist it was built
    /// from, or `None` if no artifact matched and the directory was emptied.
    pub now: Option<(u64, u64)>,
}

impl Workspace {
    /// Places the artifacts the configuration asks for, as built from
    /// changelist `cl`, and returns the directories that changed.
    ///
    /// Files of an artifact that is replaced are removed, even if they were
    /// modified; artifact directories belong to Kelp.
    pub(super) async fn sync_artifacts(
        &mut self,
        conn: &Connection,
        cache: &ContentCache,
        cl: u64,
    ) -> Result<Vec<ArtifactUpdate>> {
        let mut updates = Vec::new();
        let mounts = self.config.artifacts.clone();
        for mount in &mounts {
            mount.validate()?;
            let stream = match &mount.stream {
                Some(s) => DepotPath::parse(s)?,
                None => self.stream.clone(),
            };
            let query = ArtifactQuery {
                name: mount.name.clone(),
                at: Some(cl),
                exact: mount.exact,
                tags: mount.tags.clone(),
            };
            let found = artifact::find(conn, &stream, &query).await?;
            let have = self.state.artifacts.get(&mount.to);
            if have.map(|h| h.id) == found.as_ref().map(|a| a.id) {
                continue;
            }
            let files = match &found {
                Some(a) => artifact::files(conn, a.id).await?,
                None => Vec::new(),
            };
            let files: BTreeMap<_, _> = files.into_iter().map(|(p, f)| (p, version(f))).collect();
            self.place(cache, conn, mount, &files).await?;
            match &found {
                Some(a) => {
                    let have = ArtifactHave {
                        id: a.id,
                        cl: a.cl,
                        files,
                    };
                    self.state.artifacts.insert(mount.to.clone(), have);
                }
                None => {
                    self.state.artifacts.remove(&mount.to);
                }
            }
            updates.push(ArtifactUpdate {
                to: mount.to.clone(),
                name: mount.name.clone(),
                now: found.map(|a| (a.id, a.cl)),
            });
        }
        let configured: Vec<&str> = mounts.iter().map(|m| m.to.as_str()).collect();
        let dropped: Vec<String> = self
            .state
            .artifacts
            .keys()
            .filter(|to| !configured.contains(&to.as_str()))
            .cloned()
            .collect();
        for to in dropped {
            let mount = ArtifactMount {
                name: String::new(),
                to: to.clone(),
                stream: None,
                exact: false,
                tags: BTreeMap::new(),
            };
            self.place(cache, conn, &mount, &BTreeMap::new()).await?;
            self.state.artifacts.remove(&to);
            updates.push(ArtifactUpdate {
                to,
                name: String::new(),
                now: None,
            });
        }
        Ok(updates)
    }

    /// Makes the directory of `mount` hold exactly `files`.
    async fn place(
        &self,
        cache: &ContentCache,
        conn: &Connection,
        mount: &ArtifactMount,
        files: &BTreeMap<String, kelp_workspace::FileVersion>,
    ) -> Result<()> {
        let dir = self.abs(&mount.to);
        let old = self.state.artifacts.get(&mount.to).map(|h| &h.files);
        for rel in old.into_iter().flat_map(|f| f.keys()) {
            if !files.contains_key(rel) {
                remove_file(&dir.join(rel), &self.root)?;
            }
        }
        let changed: Vec<_> = files
            .iter()
            .filter(|(rel, f)| old.and_then(|o| o.get(*rel)) != Some(f) || !dir.join(rel).exists())
            .collect();
        let contents: Vec<_> = changed.iter().map(|(_, f)| f.content).collect();
        cache.fetch(conn, &contents).await?;
        for (rel, _) in &changed {
            let abs = dir.join(rel);
            let ours = old.is_some_and(|o| o.contains_key(*rel));
            if !ours && fs::symlink_metadata(&abs).is_ok() {
                return Err(Error::Invalid(format!(
                    "{} is in artifact directory {} but not part of the artifact; move it away",
                    abs.display(),
                    mount.to
                )));
            }
        }
        let tmp = self.tmp_dir();
        let bytes = changed.iter().map(|(_, f)| f.size).sum();
        let mut meter = cache.meter(Phase::Write, changed.len() as u64, Some(bytes));
        for (rel, f) in changed {
            meter.add(1, f.size);
            let abs = dir.join(rel);
            cache
                .materialize(&f.content, f.executable, &abs, &tmp)
                .await?;
            set_read_only(&abs, true)?;
        }
        Ok(())
    }

    /// Returns `true` if the workspace path `local` is in an artifact directory.
    pub(crate) fn in_artifact_dir(&self, local: &str) -> bool {
        let local = local.to_lowercase();
        self.config.artifacts.iter().any(|m| {
            let to = m.to.to_lowercase();
            local == to || local.strip_prefix(&to).is_some_and(|r| r.starts_with('/'))
        })
    }
}
