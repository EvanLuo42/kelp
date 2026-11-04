//! Where each piece of content appears, so that a read of content by its
//! hash can be checked against the paths it appears at.
//!
//! The index maps a content id to references of the form `f:<path>` for a
//! file revision or shelved file, `a:<stream>` for a file of an artifact
//! built from the stream, and `p` for a plugin component, which every user
//! may read. Content a stream inherited from its parent when it was created
//! is found through the parent's revisions and the tree the stream started
//! with, so creating a stream does not copy references.

use kelp_core::{ContentId, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use super::nodes::TxnNodes;
use super::{ARTIFACTS, CONTENT_REFS, PLUGINS, REVISIONS, Reader, SHELVES, dec};
use crate::record::{Artifact, PluginRecord, Revision, Shelf, Stream};
use crate::{Dirs, EntryKind, Result};

/// A place where content appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentRef {
    /// A revision or shelved version of the file at this path.
    File(DepotPath),
    /// A file of an artifact built from this stream.
    Artifact(DepotPath),
    /// A plugin component.
    Plugin,
}

impl ContentRef {
    fn key(&self) -> String {
        match self {
            Self::File(path) => format!("f:{}", path.key()),
            Self::Artifact(stream) => format!("a:{}", stream.key()),
            Self::Plugin => "p".to_owned(),
        }
    }

    fn parse(key: &str) -> Option<Self> {
        match key.split_once(':') {
            Some(("f", path)) => DepotPath::parse(&format!("//{path}")).ok().map(Self::File),
            Some(("a", stream)) => DepotPath::parse(&format!("//{stream}"))
                .ok()
                .map(Self::Artifact),
            None if key == "p" => Some(Self::Plugin),
            _ => None,
        }
    }
}

/// Records that `content` appears at `place`.
pub(super) fn add(txn: &WriteTransaction, content: &ContentId, place: &ContentRef) -> Result<()> {
    txn.open_table(CONTENT_REFS)?
        .insert((content.0.0.as_slice(), place.key().as_str()), ())?;
    Ok(())
}

/// Forgets that `content` appears at `place`.
pub(super) fn remove(
    txn: &WriteTransaction,
    content: &ContentId,
    place: &ContentRef,
) -> Result<()> {
    txn.open_table(CONTENT_REFS)?
        .remove((content.0.0.as_slice(), place.key().as_str()))?;
    Ok(())
}

/// Indexes the content of every revision, shelf, artifact and plugin, for a
/// database from before the index existed.
pub(super) fn backfill(txn: &WriteTransaction) -> Result<()> {
    let mut places = Vec::new();
    for row in txn.open_table(REVISIONS)?.iter()? {
        let rev: Revision = dec("revisions", row?.1.value())?;
        if let Some(content) = rev.content {
            places.push((content, ContentRef::File(rev.path)));
        }
    }
    for row in txn.open_table(SHELVES)?.iter()? {
        let shelf: Shelf = dec("shelves", row?.1.value())?;
        for file in &shelf.contents.files {
            if let Some(data) = file.action.data() {
                places.push((data.content, ContentRef::File(file.path.clone())));
            }
        }
    }
    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    for row in txn.open_table(ARTIFACTS)?.iter()? {
        let artifact: Artifact = dec("artifacts", row?.1.value())?;
        for file in dirs.diff_paths(None, artifact.root)? {
            if let Some(EntryKind::File { content, .. }) = file.new {
                places.push((content, ContentRef::Artifact(artifact.stream.clone())));
            }
        }
    }
    drop(nodes);
    for row in txn.open_table(PLUGINS)?.iter()? {
        let plugin: PluginRecord = dec("plugins", row?.1.value())?;
        places.push((plugin.content, ContentRef::Plugin));
    }
    for (content, place) in places {
        add(txn, &content, &place)?;
    }
    Ok(())
}

impl Reader {
    /// Returns the places where `content` appears.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`](crate::Error::Db) if the table cannot be read.
    pub fn content_refs(&self, content: &ContentId) -> Result<Vec<ContentRef>> {
        let table = self.txn().open_table(CONTENT_REFS)?;
        let key = content.0.0.as_slice();
        let mut out = Vec::new();
        for row in table.range((key, "")..)? {
            let (k, _) = row?;
            let (id, place) = k.value();
            if id != key {
                break;
            }
            out.extend(ContentRef::parse(place));
        }
        Ok(out)
    }

    /// Returns `true` if `content` appears somewhere the reader may look:
    /// at a file path for which `can_read` holds, in an artifact of a stream
    /// for which `sees` holds, or in a plugin.
    ///
    /// A file a stream inherited from its parent counts at the stream's path
    /// too, while the stream still started from that content.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`](crate::Error::Db) or
    /// [`Error::CorruptRecord`](crate::Error::CorruptRecord) if a table
    /// cannot be read.
    pub fn content_visible(
        &self,
        content: &ContentId,
        can_read: impl Fn(&DepotPath) -> bool,
        sees: impl Fn(&DepotPath) -> bool,
    ) -> Result<bool> {
        let refs = self.content_refs(content)?;
        let mut hidden = Vec::new();
        for place in refs {
            match place {
                ContentRef::Plugin => return Ok(true),
                ContentRef::Artifact(stream) if sees(&stream) => return Ok(true),
                ContentRef::Artifact(_) => {}
                ContentRef::File(path) if can_read(&path) => return Ok(true),
                ContentRef::File(path) => hidden.push(path),
            }
        }
        if hidden.is_empty() {
            return Ok(false);
        }
        let streams = self.streams()?;
        let nodes = self.nodes()?;
        let dirs = Dirs::new(&nodes);
        for path in hidden {
            let Some(origin) = streams.iter().find(|s| owns(s, &path)) else {
                continue;
            };
            let rel = super::stream_relative(&origin.path, &path).expect("the stream owns it");
            for stream in &streams {
                let other_depot = stream.path.segments().next() != origin.path.segments().next();
                if stream.path == origin.path || other_depot {
                    continue;
                }
                let Ok(there) = stream.path.join(&rel) else {
                    continue;
                };
                if !can_read(&there) {
                    continue;
                }
                let started = dirs.lookup(stream.base_root, &rel)?.map(|e| e.kind);
                if matches!(started, Some(EntryKind::File { content: c, .. }) if c == *content) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn owns(stream: &Stream, path: &DepotPath) -> bool {
    path.starts_with(&stream.path) && *path != stream.path
}
