use std::collections::HashMap;

use kelp_core::{ClNumber, DepotPath, TreeId};
use redb::{ReadableTable, WriteTransaction};

use super::nodes::TxnNodes;
use super::{
    CHANGES, META, OBLITERATED, REVIEWS, REVISIONS, SHELVES, STREAM_CHANGES, STREAM_ROOTS, STREAMS,
    VIEWS, dec, enc, stream_relative,
};
use crate::record::{Changelist, Review, Shelf, Stream};
use crate::{Dirs, EntryKind, Error, Result};

/// A parsed obliterate argument.
struct Pattern {
    path: DepotPath,
    /// Whether the pattern names a directory (`/...`).
    recursive: bool,
    /// Whether the pattern was derived from one naming a parent stream,
    /// rather than given.
    derived: bool,
}

impl Pattern {
    fn parse(s: &str) -> Result<Self> {
        let (path, recursive) = match s.strip_suffix("/...") {
            Some(dir) => (dir, true),
            None => (s, false),
        };
        let path = DepotPath::parse(path)?;
        if path.segments().count() < 3 {
            return Err(Error::Invalid(format!(
                "obliterate {s}: name files or directories inside a stream"
            )));
        }
        Ok(Self {
            path,
            recursive,
            derived: false,
        })
    }

    fn matches(&self, path: &DepotPath) -> bool {
        if self.recursive {
            path.starts_with(&self.path) && *path != self.path
        } else {
            *path == self.path
        }
    }
}

pub(super) fn apply(txn: &WriteTransaction, args: &[String]) -> Result<()> {
    let named: Vec<Pattern> = args
        .iter()
        .map(|a| Pattern::parse(a))
        .collect::<Result<_>>()?;
    let streams: Vec<Stream> = {
        let table = txn.open_table(STREAMS)?;
        table
            .iter()?
            .map(|r| dec("streams", r?.1.value()))
            .collect::<Result<_>>()?
    };
    let patterns = with_descendants(named, &streams)?;
    let matches = |p: &DepotPath| patterns.iter().any(|pat| pat.matches(p));

    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    // A pattern must say what it names, or trees and history would disagree
    // about what was removed.
    for pattern in &patterns {
        for stream in &streams {
            let Some(rel) = stream_relative(&stream.path, &pattern.path) else {
                continue;
            };
            let is_dir = match dirs.lookup(stream.root, &rel)? {
                Some(entry) => matches!(entry.kind, EntryKind::Dir { .. }),
                None => continue,
            };
            if is_dir != pattern.recursive && !pattern.derived {
                let hint = match is_dir {
                    true => format!(
                        "{} is a directory; write {}/...",
                        pattern.path, pattern.path
                    ),
                    false => format!("{} is a file; leave out /...", pattern.path),
                };
                return Err(Error::Invalid(hint));
            }
        }
    }

    let mut revisions = txn.open_table(REVISIONS)?;
    for pattern in &patterns {
        let key = pattern.path.key().to_owned();
        let (start, prefix) = match pattern.recursive {
            true => (format!("{key}/"), format!("{key}/")),
            false => (key.clone(), String::new()),
        };
        let doomed: Vec<(String, u32)> = revisions
            .range((start.as_str(), 0u32)..)?
            .map(|e| e.map(|(k, _)| (k.value().0.to_owned(), k.value().1)))
            .take_while(|e| {
                e.as_ref().is_ok_and(|(p, _)| match pattern.recursive {
                    true => p.starts_with(&prefix),
                    false => *p == key,
                })
            })
            .collect::<std::result::Result<_, _>>()?;
        for (path, rev) in doomed {
            revisions.remove((path.as_str(), rev))?;
        }
    }
    drop(revisions);
    txn.open_table(super::MOVES)?.retain(|(to, _), from| {
        let to = DepotPath::parse(&format!("//{to}"));
        let from = postcard::from_bytes::<DepotPath>(from);
        !(to.is_ok_and(|p| matches(&p)) || from.is_ok_and(|p| matches(&p)))
    })?;
    for mut stream in streams {
        let rels: Vec<String> = patterns
            .iter()
            .filter_map(|p| stream_relative(&stream.path, &p.path))
            .collect();
        if rels.is_empty() {
            continue;
        }
        let mut rewritten: HashMap<TreeId, TreeId> = HashMap::new();
        let mut rewrite = |root: TreeId| -> Result<TreeId> {
            if let Some(done) = rewritten.get(&root) {
                return Ok(*done);
            }
            let mut tree = root;
            for rel in &rels {
                tree = dirs.remove_path(tree, rel)?;
            }
            rewritten.insert(root, tree);
            Ok(tree)
        };
        let numbers: Vec<u64> = {
            let index = txn.open_table(STREAM_CHANGES)?;
            let key = stream.path.key();
            index
                .range((key, 0u64)..=(key, u64::MAX))?
                .map(|r| Ok(r?.0.value().1))
                .collect::<Result<_>>()?
        };
        let mut changes = txn.open_table(CHANGES)?;
        let mut roots = txn.open_table(STREAM_ROOTS)?;
        for number in numbers {
            // Changelists of other streams that changed this one through a
            // writable import keep this stream's tree separately.
            let separate = roots
                .get((stream.path.key(), number))?
                .map(|v| super::imports::decode_tree(v.value()))
                .transpose()?;
            if let Some(root) = separate {
                let root = rewrite(root)?;
                roots.insert((stream.path.key(), number), root.0.0.as_slice())?;
                continue;
            }
            let mut record: Changelist = match changes.get(number)? {
                Some(v) => dec("changes", v.value())?,
                None => return Err(Error::NotFound(format!("changelist {}", ClNumber(number)))),
            };
            record.root = rewrite(record.root)?;
            record.changes.retain(|c| !matches(&c.path));
            changes.insert(number, enc(&record).as_slice())?;
        }
        stream.root = rewrite(stream.root)?;
        stream.base_root = rewrite(stream.base_root)?;
        txn.open_table(STREAMS)?
            .insert(stream.path.key(), enc(&stream).as_slice())?;
    }

    drop(nodes);
    let mut emptied = Vec::new();
    let mut shelves = txn.open_table(SHELVES)?;
    let all: Vec<Shelf> = shelves
        .iter()?
        .map(|r| dec("shelves", r?.1.value()))
        .collect::<Result<_>>()?;
    for mut shelf in all {
        let before = shelf.contents.files.len();
        shelf.contents.files.retain(|f| !matches(&f.path));
        match (shelf.contents.files.len(), before) {
            (0, _) => {
                shelves.remove(shelf.number.0)?;
                emptied.push(shelf.number);
            }
            (after, before) if after != before => {
                shelves.insert(shelf.number.0, enc(&shelf).as_slice())?;
            }
            _ => {}
        }
    }
    drop(shelves);
    for shelf in emptied {
        super::governance::reviews::shelf_deleted(txn, shelf)?;
    }
    {
        let mut reviews = txn.open_table(REVIEWS)?;
        let mut changed = Vec::new();
        for row in reviews.iter()? {
            let (k, v) = row?;
            let mut review: Review = dec("reviews", v.value())?;
            let mut found = false;
            for version in &mut review.versions {
                let before = version.files.len();
                version.files.retain(|f| !matches(&f.path));
                found |= version.files.len() != before;
            }
            if found {
                changed.push((k.value(), enc(&review)));
            }
        }
        for (k, v) in changed {
            reviews.insert(k, v.as_slice())?;
        }
    }
    // Views hold the trees of their stream and of its imports as they were
    // then; remove what was obliterated from either.
    let views: Vec<(String, u64)> = txn
        .open_table(VIEWS)?
        .iter()?
        .map(|r| {
            let (k, _) = r?;
            let (stream, cl) = k.value();
            Ok((stream.to_owned(), cl))
        })
        .collect::<Result<_>>()?;
    let nodes = TxnNodes::new(txn)?;
    let dirs = Dirs::new(&nodes);
    for (key, cl) in views {
        let stream = DepotPath::parse(&format!("//{key}"))?;
        let imports = super::imports::view_imports(txn, &stream, ClNumber(cl))?;
        let mut rels = Vec::new();
        for pattern in &patterns {
            rels.extend(stream_relative(&stream, &pattern.path));
            for import in &imports {
                let from = match import.from.is_empty() {
                    true => import.source.clone(),
                    false => import.source.join(&import.from)?,
                };
                if pattern.path == from {
                    rels.push(import.to.clone());
                } else if let Some(inner) = stream_relative(&from, &pattern.path) {
                    rels.push(format!("{}/{inner}", import.to));
                }
            }
        }
        if rels.is_empty() {
            continue;
        }
        let mut root = match txn.open_table(VIEWS)?.get((key.as_str(), cl))? {
            Some(v) => super::imports::decode_tree(v.value())?,
            None => continue,
        };
        for rel in &rels {
            root = dirs.remove_path(root, rel)?;
        }
        txn.open_table(VIEWS)?
            .insert((key.as_str(), cl), root.0.0.as_slice())?;
    }
    drop(nodes);
    // Trees of old changelists now have new nodes, which a collection
    // that marked before this did not see; see `MetaDb::sweep_nodes`.
    let next = super::next_cl(txn)?;
    txn.open_table(META)?.insert(OBLITERATED, next.0)?;
    Ok(())
}

/// Adds, for each pattern inside a stream, the same path in every stream
/// branched from it, directly or not: those inherited the files when they
/// were created, and a copy-up would bring them back.
fn with_descendants(named: Vec<Pattern>, streams: &[Stream]) -> Result<Vec<Pattern>> {
    let descends = |stream: &Stream, ancestor: &DepotPath| {
        let mut parent = stream.parent.clone();
        while let Some(p) = parent {
            if p == *ancestor {
                return true;
            }
            parent = streams
                .iter()
                .find(|s| s.path == p)
                .and_then(|s| s.parent.clone());
        }
        false
    };
    let mut patterns = Vec::new();
    for pattern in named {
        for stream in streams {
            let Some(rel) = streams
                .iter()
                .find(|s| {
                    stream_relative(&s.path, &pattern.path).is_some() && descends(stream, &s.path)
                })
                .and_then(|s| stream_relative(&s.path, &pattern.path))
            else {
                continue;
            };
            patterns.push(Pattern {
                path: stream.path.join(&rel)?,
                recursive: pattern.recursive,
                derived: true,
            });
        }
        patterns.push(pattern);
    }
    Ok(patterns)
}
