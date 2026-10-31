use kelp_core::{ClNumber, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use super::apply::Origin;
use super::nodes::TxnNodes;
use super::{
    ARTIFACT_INDEX, ARTIFACT_RETENTION, ARTIFACTS, MANIFESTS, META, MetaDb, Reader, STREAMS, dec,
    enc, name_key,
};
use crate::record::Artifact;
use crate::{ArtifactContents, Dirs, EntryKind, Error, LogIndex, Result};

pub(super) const NEXT_ARTIFACT: &str = "next_artifact";

impl MetaDb {
    /// Publishes an artifact and returns its log index and id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream or changelist does not
    /// exist, [`Error::Invalid`] if the artifact has no files or bad paths,
    /// [`Error::MissingContent`] if a manifest is missing, and [`Error::Db`]
    /// if the transaction fails.
    pub fn publish_artifact(&self, contents: ArtifactContents) -> Result<(LogIndex, u64)> {
        match self.propose(&super::Proposal::PublishArtifact(contents))? {
            super::Outcome::Published { index, id } => Ok((index, id)),
            other => unreachable!("publishing produced {other:?}"),
        }
    }
}

pub(super) fn publish(
    txn: &WriteTransaction,
    id: u64,
    contents: &ArtifactContents,
    origin: Origin,
) -> Result<()> {
    let next = txn
        .open_table(META)?
        .get(NEXT_ARTIFACT)?
        .map_or(1, |v| v.value());
    if id != next {
        return Err(Error::Invalid(format!(
            "artifact {id} is not the next id {next}"
        )));
    }
    if contents.files.is_empty() {
        return Err(Error::Invalid("an artifact needs at least one file".into()));
    }
    super::apply::validate_name("artifact", &contents.name)?;
    if txn
        .open_table(STREAMS)?
        .get(contents.stream.key())?
        .is_none()
    {
        return Err(Error::NotFound(format!("stream {}", contents.stream)));
    }
    let mut folded = std::collections::HashSet::new();
    for (path, _) in &contents.files {
        if !folded.insert(crate::tree::fold(path)) {
            return Err(Error::DuplicateEntry(path.clone()));
        }
    }
    let manifests = txn.open_table(MANIFESTS)?;
    let mut missing = Vec::new();
    for (path, data) in &contents.files {
        DepotPath::parse(&format!("//artifact/{path}"))?;
        if origin == Origin::Local && manifests.get(data.content.0.0.as_slice())?.is_none() {
            missing.push(path.clone());
        }
    }
    if !missing.is_empty() {
        return Err(Error::MissingContent(missing));
    }
    drop(manifests);

    let root = {
        let nodes = TxnNodes::new(txn)?;
        let dirs = Dirs::new(&nodes);
        let files = contents
            .files
            .iter()
            .map(|(p, d)| {
                (
                    p.clone(),
                    Some(EntryKind::File {
                        content: d.content,
                        size: d.size,
                        executable: d.executable,
                    }),
                )
            })
            .collect();
        dirs.update_paths(dirs.empty()?, files)?
    };
    let artifact = Artifact::from_contents(id, root, contents);
    let (stream, name) = (contents.stream.key().to_owned(), name_key(&contents.name));
    txn.open_table(ARTIFACTS)?
        .insert(id, enc(&artifact).as_slice())?;
    txn.open_table(ARTIFACT_INDEX)?
        .insert((stream.as_str(), name.as_str(), contents.cl.0, id), ())?;
    txn.open_table(META)?.insert(NEXT_ARTIFACT, id + 1)?;

    enforce_retention(txn, &stream, &name)
}

/// Deletes the unpinned artifacts of `(stream, name)` beyond its retention
/// limit, oldest changelist first.
fn enforce_retention(txn: &WriteTransaction, stream: &str, name: &str) -> Result<()> {
    let keep = txn
        .open_table(ARTIFACT_RETENTION)?
        .get((stream, name))?
        .map(|v| v.value());
    let Some(keep) = keep else {
        return Ok(());
    };
    let ids: Vec<u64> = {
        let index = txn.open_table(ARTIFACT_INDEX)?;
        let range = index.range((stream, name, 0u64, 0u64)..=(stream, name, u64::MAX, u64::MAX))?;
        range
            .rev()
            .map(|r| Ok(r?.0.value().3))
            .collect::<Result<_>>()?
    };
    let mut kept = 0;
    for old in ids {
        let record: Artifact = match txn.open_table(ARTIFACTS)?.get(old)? {
            Some(v) => dec("artifacts", v.value())?,
            None => continue,
        };
        if record.pinned {
            continue;
        }
        if kept < keep {
            kept += 1;
        } else {
            delete(txn, old)?;
        }
    }
    Ok(())
}

pub(super) fn pin(txn: &WriteTransaction, id: u64, pinned: bool) -> Result<()> {
    let mut table = txn.open_table(ARTIFACTS)?;
    let mut record: Artifact = match table.get(id)? {
        Some(v) => dec("artifacts", v.value())?,
        None => return Err(Error::NotFound(format!("artifact {id}"))),
    };
    record.pinned = pinned;
    table.insert(id, enc(&record).as_slice())?;
    Ok(())
}

pub(super) fn delete(txn: &WriteTransaction, id: u64) -> Result<()> {
    let record: Artifact = match txn.open_table(ARTIFACTS)?.remove(id)? {
        Some(v) => dec("artifacts", v.value())?,
        None => return Err(Error::NotFound(format!("artifact {id}"))),
    };
    let name = name_key(&record.name);
    txn.open_table(ARTIFACT_INDEX)?.remove((
        record.stream.key(),
        name.as_str(),
        record.cl.0,
        id,
    ))?;
    Ok(())
}

pub(super) fn set_retention(
    txn: &WriteTransaction,
    stream: &DepotPath,
    name: &str,
    keep: Option<u32>,
) -> Result<()> {
    let name = name_key(name);
    {
        let mut table = txn.open_table(ARTIFACT_RETENTION)?;
        match keep {
            Some(n) => table.insert((stream.key(), name.as_str()), n)?,
            None => table.remove((stream.key(), name.as_str()))?,
        };
    }
    enforce_retention(txn, stream.key(), &name)
}

impl Reader {
    /// Returns artifact `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn artifact(&self, id: u64) -> Result<Option<Artifact>> {
        let table = self.txn().open_table(ARTIFACTS)?;
        table
            .get(id)?
            .map(|v| dec("artifacts", v.value()))
            .transpose()
    }

    /// Returns all artifacts, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn all_artifacts(&self) -> Result<Vec<Artifact>> {
        let table = self.txn().open_table(ARTIFACTS)?;
        table
            .iter()?
            .map(|r| dec("artifacts", r?.1.value()))
            .collect()
    }

    /// Returns the artifacts named `name` built from `stream` at or before
    /// changelist `at`, newest changelist first, that carry `tags`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn artifacts(
        &self,
        stream: &DepotPath,
        name: &str,
        at: Option<ClNumber>,
        tags: &[(String, String)],
    ) -> Result<Vec<Artifact>> {
        let index = self.txn().open_table(ARTIFACT_INDEX)?;
        let name = name_key(name);
        let upper = at.map_or(u64::MAX, |c| c.0);
        let range = index.range(
            (stream.key(), name.as_str(), 0u64, 0u64)
                ..=(stream.key(), name.as_str(), upper, u64::MAX),
        )?;
        let mut out = Vec::new();
        for entry in range.rev() {
            let id = entry?.0.value().3;
            if let Some(artifact) = self.artifact(id)?
                && artifact.has_tags(tags)
            {
                out.push(artifact);
            }
        }
        Ok(out)
    }
}
