use kelp_core::{ClNumber, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use super::{CHANGES, MOVES, Reader, dec, enc, stream_relative};
use crate::record::{Changelist, Revision};
use crate::{Error, FileAction, Result};

/// How many moves a file's history is followed through.
const MAX_MOVES: usize = 64;

pub(super) fn record(
    txn: &WriteTransaction,
    cl: ClNumber,
    moves: &[(DepotPath, DepotPath)],
) -> Result<()> {
    let change: Changelist = match txn.open_table(CHANGES)?.get(cl.0)? {
        Some(v) => dec("changes", v.value())?,
        None => return Err(Error::NotFound(format!("changelist {cl}"))),
    };
    let imports = super::imports::load(txn, &change.stream)?;
    // Revisions are kept under the path a change stands for, which below
    // an import is the path in its source.
    let real = |path: &DepotPath| -> Result<DepotPath> {
        let Some(rel) = stream_relative(&change.stream, path) else {
            return Ok(path.clone());
        };
        match super::imports::route(&imports, &rel) {
            Some((import, source_rel)) if source_rel.is_empty() => Ok(import.source.clone()),
            Some((import, source_rel)) => Ok(import.source.join(&source_rel)?),
            None => Ok(path.clone()),
        }
    };
    let mut table = txn.open_table(MOVES)?;
    for (from, to) in moves {
        let deleted = change
            .changes
            .iter()
            .any(|c| c.path == *from && matches!(c.action, FileAction::Delete { .. }));
        let added = change
            .changes
            .iter()
            .any(|c| c.path == *to && matches!(c.action, FileAction::Add { .. }));
        if !deleted || !added {
            return Err(Error::Invalid(format!(
                "changelist {cl} does not delete {from} and add {to}"
            )));
        }
        let (from, to) = (real(from)?, real(to)?);
        table.insert((to.key(), cl.0), enc(&from).as_slice())?;
    }
    Ok(())
}

impl Reader {
    /// Returns the revisions of the file at `path`, newest first, followed
    /// by those of the files it was moved from, each before the move.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn history(&self, path: &DepotPath) -> Result<Vec<Revision>> {
        let moves = self.txn().open_table(MOVES)?;
        let mut history = self.revisions(path)?;
        let mut current = path.clone();
        for _ in 0..MAX_MOVES {
            let Some(first) = history.iter().rev().find(|r| r.path == current) else {
                break;
            };
            let Some(from) = moves.get((current.key(), first.cl.0))? else {
                break;
            };
            let from: DepotPath = dec("moves", from.value())?;
            let before = first.cl;
            history.extend(self.revisions(&from)?.into_iter().filter(|r| r.cl < before));
            current = from;
        }
        Ok(history)
    }
}
