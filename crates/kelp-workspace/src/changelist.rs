//! Pending changelists local to a workspace.
//!
//! Every opened file belongs to one changelist, its owner. Text files that
//! allow it may additionally assign individual hunks to other changelists;
//! hunks without an assignment follow the file's owner.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kelp_core::{ClNumber, DepotPath, LocalClId};
use serde::{Deserialize, Serialize};

use crate::hunk::{self, HunkAnchor};
use crate::{Error, Result};

/// What an opened file will do when its changelist is submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenAction {
    Add,
    Edit,
    Delete,
    MoveFrom,
    MoveTo,
}

/// A pending changelist.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PendingChangelist {
    pub description: String,
    /// The server changelist number, once shelved.
    pub shelf: Option<ClNumber>,
    /// The merge-down this changelist completes: the source stream and the
    /// last of its changelists being merged.
    pub integration: Option<(DepotPath, u64)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpenFile {
    action: OpenAction,
    owner: LocalClId,
    splittable: bool,
    hunk_owners: HashMap<HunkAnchor, LocalClId>,
}

impl OpenFile {
    fn owner_of(&self, anchor: &HunkAnchor) -> LocalClId {
        self.hunk_owners.get(anchor).copied().unwrap_or(self.owner)
    }
}

/// The part of an opened file that belongs to a changelist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Whole,
    /// Only these hunks of the file's current diff.
    Hunks(BTreeSet<HunkAnchor>),
}

/// The opened files and pending changelists of a workspace.
///
/// The default changelist, [`LocalClId::DEFAULT`], always exists.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingChanges {
    next_id: u32,
    changelists: BTreeMap<LocalClId, PendingChangelist>,
    files: BTreeMap<DepotPath, OpenFile>,
}

impl Default for PendingChanges {
    fn default() -> Self {
        Self {
            next_id: 1,
            changelists: BTreeMap::from([(LocalClId::DEFAULT, PendingChangelist::default())]),
            files: BTreeMap::new(),
        }
    }
}

impl PendingChanges {
    /// Creates a changelist and returns its id.
    pub fn create(&mut self, description: impl Into<String>) -> LocalClId {
        let id = LocalClId(self.next_id);
        self.next_id += 1;
        self.changelists.insert(
            id,
            PendingChangelist {
                description: description.into(),
                ..PendingChangelist::default()
            },
        );
        id
    }

    /// Returns the changelist `id`, if it exists.
    pub fn changelist(&self, id: LocalClId) -> Option<&PendingChangelist> {
        self.changelists.get(&id)
    }

    /// Returns the changelist `id` for modification.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchChangelist`] if it does not exist.
    pub fn changelist_mut(&mut self, id: LocalClId) -> Result<&mut PendingChangelist> {
        self.changelists
            .get_mut(&id)
            .ok_or(Error::NoSuchChangelist(id))
    }

    /// Deletes a changelist, moving its files and hunks to the default changelist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchChangelist`] if `id` does not exist or is the
    /// default changelist.
    pub fn delete(&mut self, id: LocalClId) -> Result<()> {
        if id == LocalClId::DEFAULT || self.changelists.remove(&id).is_none() {
            return Err(Error::NoSuchChangelist(id));
        }
        for f in self.files.values_mut() {
            if f.owner == id {
                f.owner = LocalClId::DEFAULT;
            }
            f.hunk_owners.retain(|_, cl| *cl != id);
        }
        Ok(())
    }

    /// Opens `path` for `action` in changelist `cl`, replacing any previous opening.
    ///
    /// `splittable` says whether the file may later be split by hunk; it only
    /// takes effect for [`OpenAction::Edit`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoSuchChangelist`] if `cl` does not exist.
    pub fn open(
        &mut self,
        path: DepotPath,
        action: OpenAction,
        cl: LocalClId,
        splittable: bool,
    ) -> Result<()> {
        self.ensure(cl)?;
        let splittable = splittable && action == OpenAction::Edit;
        self.files.insert(
            path,
            OpenFile {
                action,
                owner: cl,
                splittable,
                hunk_owners: HashMap::new(),
            },
        );
        Ok(())
    }

    /// Removes `path` from its changelist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotOpened`] if the file is not opened.
    pub fn revert(&mut self, path: &DepotPath) -> Result<()> {
        self.files
            .remove(path)
            .map(drop)
            .ok_or_else(|| Error::NotOpened(path.clone()))
    }

    /// Moves the whole file, including all of its hunks, to changelist `cl`.
    ///
    /// # Errors
    ///
    /// Returns an error if `cl` does not exist or the file is not opened.
    pub fn reopen(&mut self, path: &DepotPath, cl: LocalClId) -> Result<()> {
        self.ensure(cl)?;
        let f = self.file_mut(path)?;
        f.owner = cl;
        f.hunk_owners.clear();
        Ok(())
    }

    /// Assigns one hunk of an opened text file to changelist `cl`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotSplittable`] if the file may not be split, or an
    /// error if `cl` does not exist or the file is not opened.
    pub fn assign_hunk(
        &mut self,
        path: &DepotPath,
        anchor: HunkAnchor,
        cl: LocalClId,
    ) -> Result<()> {
        self.ensure(cl)?;
        let f = self.file_mut(path)?;
        if !f.splittable {
            return Err(Error::NotSplittable(path.clone()));
        }
        if cl == f.owner {
            f.hunk_owners.remove(&anchor);
        } else {
            f.hunk_owners.insert(anchor, cl);
        }
        Ok(())
    }

    /// Returns how `path` is opened, or `None` if it is not opened.
    pub fn action(&self, path: &DepotPath) -> Option<OpenAction> {
        self.files.get(path).map(|f| f.action)
    }

    /// Returns the files, or parts of files, that belong to changelist `cl`.
    ///
    /// `hunks_of` returns the current hunk anchors of a file that has hunk
    /// assignments.
    pub fn contents(
        &self,
        cl: LocalClId,
        hunks_of: impl Fn(&DepotPath) -> Vec<HunkAnchor>,
    ) -> Vec<(DepotPath, Selection)> {
        let mut out = Vec::new();
        for (path, f) in &self.files {
            if f.hunk_owners.is_empty() {
                if f.owner == cl {
                    out.push((path.clone(), Selection::Whole));
                }
                continue;
            }
            let anchors = hunks_of(path);
            let mine: BTreeSet<_> = anchors
                .iter()
                .filter(|a| f.owner_of(a) == cl)
                .copied()
                .collect();
            if mine.len() == anchors.len() && !anchors.is_empty() {
                out.push((path.clone(), Selection::Whole));
            } else if !mine.is_empty() {
                out.push((path.clone(), Selection::Hunks(mine)));
            }
        }
        out
    }

    /// Returns the content changelist `cl` would submit for an opened text file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotOpened`] if the file is not opened.
    pub fn compose_text(
        &self,
        cl: LocalClId,
        path: &DepotPath,
        base: &str,
        working: &str,
    ) -> Result<String> {
        let f = self
            .files
            .get(path)
            .ok_or_else(|| Error::NotOpened(path.clone()))?;
        let hunks = hunk::diff_hunks(base, working);
        Ok(hunk::apply_hunks(
            base,
            hunks.iter().filter(|h| f.owner_of(&h.anchor) == cl),
        ))
    }

    /// Records that changelist `cl` was submitted.
    ///
    /// Files in `fully_submitted` are closed. Other files lose their hunk
    /// assignments to `cl`. A non-default changelist that no longer owns any
    /// file is removed.
    pub fn complete_submit(&mut self, cl: LocalClId, fully_submitted: &[DepotPath]) {
        for p in fully_submitted {
            self.files.remove(p);
        }
        for f in self.files.values_mut() {
            f.hunk_owners.retain(|_, owner| *owner != cl);
        }
        if cl != LocalClId::DEFAULT {
            let still_used = self.files.values().any(|f| f.owner == cl);
            if !still_used {
                self.changelists.remove(&cl);
            }
        }
    }

    /// Returns the files that have hunks assigned to changelists other than
    /// their owner.
    pub fn split_files(&self) -> impl Iterator<Item = &DepotPath> {
        self.files
            .iter()
            .filter(|(_, f)| !f.hunk_owners.is_empty())
            .map(|(p, _)| p)
    }

    /// Returns the changelist a hunk of an opened file belongs to.
    pub fn hunk_owner(&self, path: &DepotPath, anchor: &HunkAnchor) -> Option<LocalClId> {
        self.files.get(path).map(|f| f.owner_of(anchor))
    }

    /// Returns the changelist that owns the opened file `path`.
    pub fn owner(&self, path: &DepotPath) -> Option<LocalClId> {
        self.files.get(path).map(|f| f.owner)
    }

    /// Returns all changelists, the default changelist first.
    pub fn changelists(&self) -> impl Iterator<Item = (LocalClId, &PendingChangelist)> {
        self.changelists.iter().map(|(id, cl)| (*id, cl))
    }

    /// Returns all opened files with their action and owning changelist.
    pub fn opened(&self) -> impl Iterator<Item = (&DepotPath, OpenAction, LocalClId)> {
        self.files.iter().map(|(path, f)| (path, f.action, f.owner))
    }

    fn ensure(&self, cl: LocalClId) -> Result<()> {
        self.changelists
            .contains_key(&cl)
            .then_some(())
            .ok_or(Error::NoSuchChangelist(cl))
    }

    fn file_mut(&mut self, path: &DepotPath) -> Result<&mut OpenFile> {
        self.files
            .get_mut(path)
            .ok_or_else(|| Error::NotOpened(path.clone()))
    }
}
