use std::collections::BTreeSet;
use std::path::Path;

use redb::{Database, Key, ReadTransaction, ReadableTable, TableDefinition, TableHandle, Value};

use super::*;
use crate::{Error, Result};

/// Calls `$f(src, dst, TABLE)` for every table of the database and returns
/// the set of their names.
macro_rules! for_each_table {
    ($f:ident, $src:expr, $dst:expr) => {{
        let mut done = BTreeSet::new();
        for_each_table!(@each $f, $src, $dst, done, META,
            LOG,
            NODES,
            MANIFESTS,
            USERS,
            TOKENS,
            GROUPS,
            SETTINGS,
            DEPOTS,
            TYPEMAPS,
            STREAMS,
            CHANGES,
            STREAM_CHANGES,
            REVISIONS,
            SHELVES,
            LOCKS,
            INTEGRATIONS,
            AUDIT,
            ARTIFACTS,
            ARTIFACT_INDEX,
            ARTIFACT_RETENTION,
            REVIEWS,
            WEBHOOKS,
            PLUGINS,
            ISSUE_CHANGES,
            DELIVERIES,
            PENDING_DELIVERIES,
            IMPORTS,
            IMPORTERS,
            VIEWS,
            STREAM_ROOTS,
            CONSENSUS,
            SSO_SUBJECTS,
            SSO_LINKS,
            SSO_EXPECTED,
            ALIASES,
            VIEW_IMPORTS,
            DISABLED,
            LABELS,
            MOVES);
        done
    }};
    (@each $f:ident, $src:expr, $dst:expr, $done:ident, $($table:ident),*) => {
        $($done.insert($f($src, $dst, $table)?);)*
    };
}

impl MetaDb {
    /// Writes a consistent copy of the whole database to a new file at
    /// `path`, as of one moment, while the database stays in use.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AlreadyExists`] if `path` exists, [`Error::Format`]
    /// if the database holds a table this build does not know how to copy,
    /// or [`Error::Db`] if a database cannot be read or written.
    pub fn snapshot_to(&self, path: &Path) -> Result<()> {
        if path.exists() {
            return Err(Error::AlreadyExists(path.display().to_string()));
        }
        let src = self.db.begin_read()?;
        let dst = Database::create(path)?;
        let txn = dst.begin_write()?;
        let copied = for_each_table!(copy, &src, &txn);
        for table in src.list_tables()? {
            if !copied.contains(table.name()) {
                return Err(Error::Format(format!(
                    "the database has a table {} that a snapshot would leave out",
                    table.name()
                )));
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Replaces the whole contents of the database with those of the
    /// snapshot at `path`, written by [`snapshot_to`](Self::snapshot_to),
    /// in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if the snapshot has a different format or
    /// a table this build does not know, or [`Error::Db`] if a database
    /// cannot be read or written.
    pub fn replace_with(&self, path: &Path) -> Result<()> {
        let snapshot = Database::open(path)?;
        let src = snapshot.begin_read()?;
        let format = src.open_table(META)?.get(FORMAT)?.map(|v| v.value());
        if format != Some(FORMAT_VERSION) {
            return Err(Error::Format(format!(
                "the snapshot has format {format:?}, this build {FORMAT_VERSION}"
            )));
        }
        let txn = self.db.begin_write()?;
        let replaced = for_each_table!(replace, &src, &txn);
        for table in src.list_tables()? {
            if !replaced.contains(table.name()) {
                return Err(Error::Format(format!(
                    "the snapshot has a table {} this build does not know",
                    table.name()
                )));
            }
        }
        txn.commit()?;
        Ok(())
    }
}

fn copy<K: Key + 'static, V: Value + 'static>(
    src: &ReadTransaction,
    dst: &redb::WriteTransaction,
    table: TableDefinition<K, V>,
) -> Result<String> {
    let from = src.open_table(table)?;
    let mut to = dst.open_table(table)?;
    for row in from.iter()? {
        let (k, v) = row?;
        to.insert(k.value(), v.value())?;
    }
    Ok(table.name().to_owned())
}

fn replace<K: Key + 'static, V: Value + 'static>(
    src: &ReadTransaction,
    dst: &redb::WriteTransaction,
    table: TableDefinition<K, V>,
) -> Result<String> {
    let mut to = dst.open_table(table)?;
    to.retain(|_, _| false)?;
    match src.open_table(table) {
        Ok(from) => {
            for row in from.iter()? {
                let (k, v) = row?;
                to.insert(k.value(), v.value())?;
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(table.name().to_owned())
}
