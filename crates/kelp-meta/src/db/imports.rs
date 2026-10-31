use std::collections::BTreeSet;

use kelp_core::{ClNumber, DepotPath, TreeId};
use redb::{ReadableTable, WriteTransaction};

use super::nodes::TxnNodes;
use super::{
    CHANGES, IMPORTERS, IMPORTS, META, NEXT_CL, Reader, STREAM_CHANGES, STREAM_ROOTS, STREAMS,
    VIEW_IMPORTS, VIEWS, dec, enc,
};
use crate::record::{Changelist, Import, Stream};
use crate::tree::fold;
use crate::{Dirs, EntryKind, Error, Result};

/// Returns the import that `rel`, a path relative to the importing stream,
/// falls under, with the path relative to the import's source stream.
pub fn route<'a>(imports: &'a [Import], rel: &str) -> Option<(&'a Import, String)> {
    let segments: Vec<&str> = rel.split('/').collect();
    imports.iter().find_map(|import| {
        let to: Vec<&str> = import.to.split('/').collect();
        let inside =
            segments.len() >= to.len() && segments.iter().zip(&to).all(|(a, b)| fold(a) == fold(b));
        if !inside {
            return None;
        }
        let rest = segments[to.len()..].join("/");
        let source = match (import.from.is_empty(), rest.is_empty()) {
            (true, _) => rest,
            (false, true) => import.from.clone(),
            (false, false) => format!("{}/{rest}", import.from),
        };
        Some((import, source))
    })
}

pub(super) fn set(txn: &WriteTransaction, stream: &DepotPath, imports: &[Import]) -> Result<()> {
    let record = super::load_stream(txn, stream)?;
    if has_importers(txn, stream)? && !imports.is_empty() {
        return Err(Error::Invalid(format!(
            "{stream} is imported by another stream and cannot import itself"
        )));
    }
    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    let mut mounts: Vec<String> = Vec::new();
    for import in imports {
        let to = &import.to;
        let bad_path = to.is_empty() || to.split('/').any(|s| matches!(s, "" | "." | ".."));
        if bad_path || stream.join(to).is_err() {
            return Err(Error::Invalid(format!(
                "import path {to:?} is not a directory of {stream}"
            )));
        }
        let folded = format!("{}/", fold(to));
        if mounts
            .iter()
            .any(|m| m.starts_with(&folded) || folded.starts_with(m.as_str()))
        {
            return Err(Error::Invalid(format!(
                "import path {to} overlaps another import"
            )));
        }
        mounts.push(folded);
        if import.source == *stream {
            return Err(Error::Invalid(format!("{stream} cannot import itself")));
        }
        let source = super::load_stream(txn, &import.source)?;
        if !load(txn, &source.path)?.is_empty() {
            return Err(Error::Invalid(format!(
                "{} imports other streams and cannot be imported",
                source.path
            )));
        }
        if import.writable && import.at.is_some() {
            return Err(Error::Invalid(format!(
                "the writable import of {} must follow its head",
                source.path
            )));
        }
        if dirs.lookup(record.root, to)?.is_some() {
            return Err(Error::Invalid(format!(
                "{stream} already has {to}; an import needs an empty path"
            )));
        }
        if !import.from.is_empty() {
            let root = root_at(txn, &source.path, import.at.unwrap_or(source.head))?;
            if !matches!(
                dirs.lookup(root, &import.from)?.map(|e| e.kind),
                Some(EntryKind::Dir { .. })
            ) {
                return Err(Error::Invalid(format!(
                    "{} has no directory {}",
                    source.path, import.from
                )));
            }
        }
    }
    drop(nodes);

    let old = load(txn, stream)?;
    let mut importers = txn.open_table(IMPORTERS)?;
    for import in &old {
        importers.remove((import.source.key(), stream.key()))?;
    }
    for import in imports {
        importers.insert((import.source.key(), stream.key()), ())?;
    }
    drop(importers);
    let mut table = txn.open_table(IMPORTS)?;
    match imports.is_empty() {
        true => table.remove(stream.key())?,
        false => table.insert(stream.key(), enc(&imports.to_vec()).as_slice())?,
    };
    drop(table);
    // The view changes, so it gets a changelist number of its own, and
    // clients see that there is something new to sync.
    let mut meta = txn.open_table(META)?;
    let cl = ClNumber(meta.get(NEXT_CL)?.map_or(1, |v| v.value()));
    meta.insert(NEXT_CL, cl.0 + 1)?;
    drop(meta);
    write_view(txn, stream, cl, imports)
}

/// Records the views, as of changelist `cl`, of `touched` streams that
/// import others and of the streams importing them.
pub(super) fn refresh_views(
    txn: &WriteTransaction,
    cl: ClNumber,
    touched: &[DepotPath],
) -> Result<()> {
    let mut viewers: BTreeSet<String> = BTreeSet::new();
    {
        let importers = txn.open_table(IMPORTERS)?;
        for stream in touched {
            if txn.open_table(IMPORTS)?.get(stream.key())?.is_some() {
                viewers.insert(stream.key().to_owned());
            }
            let range = importers.range((stream.key(), "")..)?;
            for row in range {
                let (key, _) = row?;
                let (source, importer) = key.value();
                if source != stream.key() {
                    break;
                }
                viewers.insert(importer.to_owned());
            }
        }
    }
    for key in viewers {
        let stream = load_stream_by_key(txn, &key)?;
        let imports = load(txn, &stream.path)?;
        write_view(txn, &stream.path, cl, &imports)?;
    }
    Ok(())
}

/// Records the view of `stream` as of `cl`: its own tree as of then, with
/// each import grafted in.
fn write_view(
    txn: &WriteTransaction,
    stream: &DepotPath,
    cl: ClNumber,
    imports: &[Import],
) -> Result<()> {
    let root = compose(txn, stream, cl, imports)?;
    txn.open_table(VIEWS)?
        .insert((stream.key(), cl.0), root.0.0.as_slice())?;
    txn.open_table(VIEW_IMPORTS)?
        .insert((stream.key(), cl.0), enc(&imports.to_vec()).as_slice())?;
    Ok(())
}

/// Returns the imports the view of `stream` at `cl` was composed with.
pub(super) fn view_imports(
    txn: &WriteTransaction,
    stream: &DepotPath,
    cl: ClNumber,
) -> Result<Vec<Import>> {
    match txn.open_table(VIEW_IMPORTS)?.get((stream.key(), cl.0))? {
        Some(v) => dec("view_imports", v.value()),
        None => load(txn, stream),
    }
}

/// Records, for views written before their imports were kept, the
/// stream's current imports, the best guess there is.
pub(super) fn record_view_imports(txn: &WriteTransaction) -> Result<()> {
    let views: Vec<(String, u64)> = txn
        .open_table(VIEWS)?
        .iter()?
        .map(|r| {
            let (k, _) = r?;
            let (stream, cl) = k.value();
            Ok((stream.to_owned(), cl))
        })
        .collect::<Result<_>>()?;
    for (key, cl) in views {
        let imports: Vec<Import> = match txn.open_table(IMPORTS)?.get(key.as_str())? {
            Some(v) => dec("imports", v.value())?,
            None => Vec::new(),
        };
        txn.open_table(VIEW_IMPORTS)?
            .insert((key.as_str(), cl), enc(&imports).as_slice())?;
    }
    Ok(())
}

pub(super) fn compose(
    txn: &WriteTransaction,
    stream: &DepotPath,
    cl: ClNumber,
    imports: &[Import],
) -> Result<TreeId> {
    let own = root_at(txn, stream, cl)?;
    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    let empty = dirs.empty()?;
    let mut grafts = Vec::new();
    for import in imports {
        let source = root_at(txn, &import.source, import.at.unwrap_or(cl))?;
        let tree = match import.from.is_empty() {
            true => Some(source),
            false => match dirs.lookup(source, &import.from)?.map(|e| e.kind) {
                Some(EntryKind::Dir { tree }) => Some(tree),
                _ => None,
            },
        };
        if let Some(tree) = tree.filter(|t| *t != empty) {
            grafts.push((import.to.clone(), Some(EntryKind::Dir { tree })));
        }
    }
    dirs.update_paths(own, grafts)
}

/// Returns the tree of `stream` as of changelist `at`.
pub(super) fn root_at(txn: &WriteTransaction, stream: &DepotPath, at: ClNumber) -> Result<TreeId> {
    let record = super::load_stream(txn, stream)?;
    if at >= record.head {
        return Ok(record.root);
    }
    let last = txn
        .open_table(STREAM_CHANGES)?
        .range((stream.key(), 0u64)..=(stream.key(), at.0))?
        .next_back()
        .transpose()?
        .map(|(k, _)| k.value().1);
    match last {
        Some(cl) => root_of_change(txn, stream, ClNumber(cl)),
        None => Ok(record.base_root),
    }
}

/// Returns the tree `stream` had after changelist `cl`, which touched it.
fn root_of_change(txn: &WriteTransaction, stream: &DepotPath, cl: ClNumber) -> Result<TreeId> {
    if let Some(v) = txn.open_table(STREAM_ROOTS)?.get((stream.key(), cl.0))? {
        return decode_tree(v.value());
    }
    let record: Changelist = match txn.open_table(CHANGES)?.get(cl.0)? {
        Some(v) => dec("changes", v.value())?,
        None => return Err(Error::NotFound(format!("changelist {cl}"))),
    };
    Ok(record.root)
}

pub(super) fn load(txn: &WriteTransaction, stream: &DepotPath) -> Result<Vec<Import>> {
    match txn.open_table(IMPORTS)?.get(stream.key())? {
        Some(v) => dec("imports", v.value()),
        None => Ok(Vec::new()),
    }
}

fn has_importers(txn: &WriteTransaction, stream: &DepotPath) -> Result<bool> {
    let importers = txn.open_table(IMPORTERS)?;
    let first = importers.range((stream.key(), "")..)?.next().transpose()?;
    Ok(first.is_some_and(|(k, _)| k.value().0 == stream.key()))
}

fn load_stream_by_key(txn: &WriteTransaction, key: &str) -> Result<Stream> {
    match txn.open_table(STREAMS)?.get(key)? {
        Some(v) => dec("streams", v.value()),
        None => Err(Error::NotFound(format!("stream {key}"))),
    }
}

pub(super) fn decode_tree(bytes: &[u8]) -> Result<TreeId> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::CorruptRecord("views"))?;
    Ok(TreeId(kelp_core::Hash(bytes)))
}

impl Reader {
    /// Returns the imports of `stream`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn imports(&self, stream: &DepotPath) -> Result<Vec<Import>> {
        match self.txn().open_table(IMPORTS)?.get(stream.key())? {
            Some(v) => dec("imports", v.value()),
            None => Ok(Vec::new()),
        }
    }

    /// Returns what clients of `stream` see as of changelist `at`, or its
    /// head: the changelist and the tree, with imports grafted in.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream does not exist, or
    /// [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn view_at(&self, stream: &DepotPath, at: Option<ClNumber>) -> Result<(ClNumber, TreeId)> {
        let own = self.stream_at(stream, at)?;
        let views = self.txn().open_table(VIEWS)?;
        let end = at.map_or(u64::MAX, |c| c.0);
        let view = views
            .range((stream.key(), 0u64)..=(stream.key(), end))?
            .next_back()
            .transpose()?;
        match view {
            // A view older than the stream's own last change belongs to
            // imports that were removed since.
            Some((k, v)) if k.value().1 >= own.0.0 => {
                Ok((ClNumber(k.value().1), decode_tree(v.value())?))
            }
            _ => Ok(own),
        }
    }

    /// Returns the import that the path of `stream` falls under, and the
    /// path it stands for in the import's source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn route(
        &self,
        stream: &DepotPath,
        path: &DepotPath,
    ) -> Result<Option<(Import, DepotPath)>> {
        let Some(rel) = super::stream_relative(stream, path) else {
            return Ok(None);
        };
        let imports = self.imports(stream)?;
        let Some((import, source_rel)) = route(&imports, &rel) else {
            return Ok(None);
        };
        let real = match source_rel.is_empty() {
            true => import.source.clone(),
            false => import.source.join(&source_rel)?,
        };
        Ok(Some((import.clone(), real)))
    }

    /// Returns the path that `path` stands for: the path in an import's
    /// source if it lies below an import of the stream containing it, or
    /// `path` itself.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn real_path(&self, path: &DepotPath) -> Result<DepotPath> {
        self.real_paths()?.real_path(path)
    }

    /// Reads every stream's imports once, to resolve many paths with
    /// [`RealPaths::real_path`] as [`real_path`](Self::real_path) would.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn real_paths(&self) -> Result<RealPaths> {
        let mut streams = Vec::new();
        for row in self.txn().open_table(IMPORTS)?.iter()? {
            let (k, v) = row?;
            let stream = DepotPath::parse(&format!("//{}", k.value()))?;
            streams.push((stream, dec("imports", v.value())?));
        }
        Ok(RealPaths { streams })
    }
}

/// The imports of every stream, from [`Reader::real_paths`](super::Reader::real_paths).
pub struct RealPaths {
    streams: Vec<(DepotPath, Vec<Import>)>,
}

impl RealPaths {
    /// Returns the path that `path` stands for; see
    /// [`Reader::real_path`](super::Reader::real_path).
    ///
    /// # Errors
    ///
    /// Returns an error if the path in an import's source is invalid.
    pub fn real_path(&self, path: &DepotPath) -> Result<DepotPath> {
        for (stream, imports) in &self.streams {
            let Some(rel) = super::stream_relative(stream, path) else {
                continue;
            };
            return Ok(match route(imports, &rel) {
                Some((import, source_rel)) if source_rel.is_empty() => import.source.clone(),
                Some((import, source_rel)) => import.source.join(&source_rel)?,
                None => path.clone(),
            });
        }
        Ok(path.clone())
    }
}
