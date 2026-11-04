use std::collections::{HashMap, HashSet};

use kelp_core::DepotPath;
use redb::{ReadableTable, WriteTransaction};

use super::{
    AUDIT, IMPORTERS, IMPORTS, INTEGRATIONS, LABELS, LOCKS, LOG, Reader, SHELVES, STREAM_OWNERS,
    STREAMS, USERS, VIEW_IMPORTS, VIEWS, dec, load_stream, name_key,
};
use crate::record::{AuditEntry, Label, Shelf, Stream, User};
use crate::{Command, Error, Result, StreamKind};

pub(super) fn delete(txn: &WriteTransaction, path: &DepotPath) -> Result<()> {
    let stream = load_stream(txn, path)?;
    let refuse = |why: &str| Err(Error::Invalid(format!("cannot delete {path}: {why}")));
    if stream.kind == StreamKind::Mainline {
        return refuse("it is a mainline");
    }
    let streams: Vec<Stream> = txn
        .open_table(STREAMS)?
        .iter()?
        .map(|r| dec("streams", r?.1.value()))
        .collect::<Result<_>>()?;
    if streams.iter().any(|s| s.parent.as_ref() == Some(path)) {
        return refuse("other streams branch from it");
    }
    if txn.open_table(IMPORTS)?.get(path.key())?.is_some() {
        return refuse("it imports other streams");
    }
    let imported = txn
        .open_table(IMPORTERS)?
        .range((path.key(), "")..)?
        .next()
        .transpose()?
        .is_some_and(|(k, _)| k.value().0 == path.key());
    if imported {
        return refuse("another stream imports it");
    }
    for row in txn.open_table(SHELVES)?.iter()? {
        let shelf: Shelf = dec("shelves", row?.1.value())?;
        if shelf.contents.stream == *path {
            return refuse(&format!(
                "shelf {} is of it; delete its shelves first",
                shelf.number
            ));
        }
    }

    txn.open_table(STREAMS)?.remove(path.key())?;
    txn.open_table(INTEGRATIONS)?
        .retain(|(target, source), _| target != path.key() && source != path.key())?;
    txn.open_table(LABELS)?
        .retain(|_, v| postcard::from_bytes::<Label>(v).map_or(true, |l| l.stream != *path))?;
    let shared = streams
        .iter()
        .any(|s| s.path != *path && s.lock_domain == stream.lock_domain);
    if !shared {
        let domain = stream.lock_domain.0.as_str();
        txn.open_table(LOCKS)?.retain(|(d, _), _| d != domain)?;
    }
    // Nobody syncs a deleted stream, and a stream created later at its
    // path must not inherit its views.
    forget_views(txn, path.key())?;
    txn.open_table(STREAM_OWNERS)?.remove(path.key())?;
    Ok(())
}

/// Deletes the views recorded for the stream keyed `key`.
pub(super) fn forget_views(txn: &WriteTransaction, key: &str) -> Result<()> {
    txn.open_table(VIEWS)?.retain(|(s, _), _| s != key)?;
    txn.open_table(VIEW_IMPORTS)?.retain(|(s, _), _| s != key)?;
    Ok(())
}

/// Records that `owner` created the stream at `path`.
pub(super) fn set_owner(txn: &WriteTransaction, path: &DepotPath, owner: &str) -> Result<()> {
    txn.open_table(STREAM_OWNERS)?.insert(path.key(), owner)?;
    Ok(())
}

impl Reader {
    /// Returns the user who created the stream at `path`, or `None` if the
    /// stream does not exist or was created before Kelp recorded it and
    /// the log no longer says.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the table cannot be read.
    pub fn stream_owner(&self, path: &DepotPath) -> Result<Option<String>> {
        Ok(self
            .txn()
            .open_table(STREAM_OWNERS)?
            .get(path.key())?
            .map(|v| v.value().to_owned()))
    }
}

/// Part of the migration to format 10: replays the log's stream creations
/// and deletions to drop the views that deleted streams left behind, even
/// where a stream was since created again at the same path, and to record
/// who created each stream.
pub(super) fn replay_stream_lives(txn: &WriteTransaction) -> Result<()> {
    let live: HashSet<String> = txn
        .open_table(STREAMS)?
        .iter()?
        .map(|r| Ok(r?.0.value().to_owned()))
        .collect::<Result<_>>()?;
    for table in [VIEWS, VIEW_IMPORTS] {
        txn.open_table(table)?
            .retain(|(s, _), _| live.contains(s))?;
    }

    // The changelist number each command allocated is not logged, so it is
    // counted as applying did: submits and new shelves use their number,
    // and setting imports takes the next one.
    let mut next = 1u64;
    let mut deleted: HashSet<String> = HashSet::new();
    let mut recreated: HashMap<String, u64> = HashMap::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut complete = true;
    {
        let log = txn.open_table(LOG)?;
        let audit = txn.open_table(AUDIT)?;
        for (i, row) in log.iter()?.enumerate() {
            let (index, bytes) = row?;
            let index = index.value();
            if index != i as u64 + 1 {
                // A log that does not start at the beginning cannot be counted.
                complete = false;
                break;
            }
            let Ok(cmd) = postcard::from_bytes::<Command>(bytes.value()) else {
                complete = false;
                break;
            };
            let actor = match audit.get(index)? {
                Some(v) => dec::<AuditEntry>("audit", v.value())?.actor,
                None => String::new(),
            };
            match cmd {
                Command::Submit { cl, .. } => next = next.max(cl.0 + 1),
                Command::Shelve { cl, .. } if cl.0 >= next => next = cl.0 + 1,
                Command::SetImports { .. } => next += 1,
                Command::CreateStream { path, .. } => {
                    let key = path.key().to_owned();
                    if deleted.remove(&key) {
                        recreated.insert(key.clone(), next);
                    }
                    if !actor.is_empty() {
                        owners.insert(key, actor);
                    }
                }
                Command::DeleteStream { path } => {
                    deleted.insert(path.key().to_owned());
                    recreated.remove(path.key());
                    owners.remove(path.key());
                }
                Command::MergeUser { from, into } => {
                    for owner in owners.values_mut() {
                        if owner.eq_ignore_ascii_case(&from) {
                            owner.clone_from(&into);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if complete {
        for (key, since) in &recreated {
            if live.contains(key) {
                for table in [VIEWS, VIEW_IMPORTS] {
                    txn.open_table(table)?
                        .retain(|(s, cl), _| s != key.as_str() || cl >= *since)?;
                }
            }
        }
    }
    let mut table = txn.open_table(STREAM_OWNERS)?;
    for (key, owner) in owners {
        if !live.contains(&key) || table.get(key.as_str())?.is_some() {
            continue;
        }
        // Spell the owner as the user is called now.
        let owner_key = name_key(&owner);
        let current = match txn.open_table(USERS)?.get(owner_key.as_str())? {
            Some(v) => dec::<User>("users", v.value())?.name,
            None => super::accounts::alias_of(txn, &owner_key)?.unwrap_or(owner),
        };
        table.insert(key.as_str(), current.as_str())?;
    }
    Ok(())
}
