use redb::ReadableTable;

use super::apply::{self, Origin};
use super::{AUDIT, LAST_INDEX, LOG, META, MetaDb, Reader, append_log, dec};
use crate::log::LogEntry;
use crate::record::AuditEntry;
use crate::{Command, Error, Result};

impl Reader {
    /// Returns up to `limit` log entries starting at index `from`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the log cannot be
    /// read.
    pub fn log_entries(&self, from: u64, limit: usize) -> Result<Vec<LogEntry>> {
        let log = self.txn().open_table(LOG)?;
        let audit = self.txn().open_table(AUDIT)?;
        let mut entries = Vec::new();
        for row in log.range(from.max(1)..)?.take(limit) {
            let (index, command) = row?;
            let index = index.value();
            let link: AuditEntry = match audit.get(index)? {
                Some(v) => dec("audit", v.value())?,
                None => return Err(Error::AuditMismatch(index)),
            };
            entries.push(LogEntry {
                index,
                actor: link.actor,
                command: command.value().to_vec(),
                hash: link.hash,
            });
        }
        Ok(entries)
    }
}

impl MetaDb {
    /// Applies an entry of the commit server's log to this replica.
    ///
    /// Entries must arrive in order. The replica recomputes the entry's audit
    /// hash, which covers the whole log before it, and rejects the entry if
    /// the hash differs, so a replica never silently diverges.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if the entry is not the next one or cannot
    /// be decoded, [`Error::AuditMismatch`] if the replica's history differs
    /// from the commit server's, or the error the command fails with.
    pub fn replicate(&self, entry: &LogEntry) -> Result<()> {
        let txn = self.db.begin_write()?;
        let last = txn
            .open_table(META)?
            .get(LAST_INDEX)?
            .map_or(0, |v| v.value());
        if entry.index != last + 1 {
            return Err(Error::Invalid(format!(
                "log entry {} does not follow {last}",
                entry.index
            )));
        }
        let cmd: Command = postcard::from_bytes(&entry.command)
            .map_err(|_| Error::Invalid(format!("log entry {} is corrupt", entry.index)))?;
        apply::apply(&txn, &cmd, Origin::Replicated, &entry.actor)?;
        append_log(&txn, &cmd, &entry.actor, Origin::Replicated)?;
        let hash = match txn.open_table(AUDIT)?.get(entry.index)? {
            Some(v) => dec::<AuditEntry>("audit", v.value())?.hash,
            None => return Err(Error::AuditMismatch(entry.index)),
        };
        if hash != entry.hash {
            return Err(Error::AuditMismatch(entry.index));
        }
        txn.commit()?;
        Ok(())
    }
}
