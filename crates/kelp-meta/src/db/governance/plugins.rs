use redb::{ReadableTable, WriteTransaction};

use crate::db::apply::Origin;
use crate::db::{MANIFESTS, PLUGINS, Reader, dec, enc, name_key};
use crate::record::PluginRecord;
use crate::{Error, Governance, Result};

pub(in crate::db) fn apply(txn: &WriteTransaction, cmd: &Governance, origin: Origin) -> Result<()> {
    match cmd {
        Governance::RegisterPlugin {
            name,
            version,
            content,
            size,
            worlds,
            by,
            at,
        } => {
            crate::db::apply::validate_name("plugin", name)?;
            crate::db::apply::validate_name("plugin version", version)?;
            let key = key(name, version);
            let mut plugins = txn.open_table(PLUGINS)?;
            if plugins.get(key.as_str())?.is_some() {
                return Err(Error::AlreadyExists(format!("plugin {name}@{version}")));
            }
            if origin == Origin::Local
                && txn
                    .open_table(MANIFESTS)?
                    .get(content.0.0.as_slice())?
                    .is_none()
            {
                return Err(Error::MissingContent(vec![format!("{name}@{version}")]));
            }
            let record = PluginRecord {
                name: name.clone(),
                version: version.clone(),
                content: *content,
                size: *size,
                worlds: worlds.clone(),
                by: by.clone(),
                at: *at,
            };
            plugins.insert(key.as_str(), enc(&record).as_slice())?;
        }
        Governance::DeletePlugin { name, version } => {
            if txn
                .open_table(PLUGINS)?
                .remove(key(name, version).as_str())?
                .is_none()
            {
                return Err(Error::NotFound(format!("plugin {name}@{version}")));
            }
        }
        _ => unreachable!("not a plugin command"),
    }
    Ok(())
}

fn key(name: &str, version: &str) -> String {
    format!("{}@{}", name_key(name), name_key(version))
}

impl Reader {
    /// Returns plugin `name` at `version`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn plugin(&self, name: &str, version: &str) -> Result<Option<PluginRecord>> {
        self.txn()
            .open_table(PLUGINS)?
            .get(key(name, version).as_str())?
            .map(|v| dec("plugins", v.value()))
            .transpose()
    }

    /// Returns all registered plugin versions, ordered by name and version.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn plugins(&self) -> Result<Vec<PluginRecord>> {
        self.txn()
            .open_table(PLUGINS)?
            .iter()?
            .map(|r| dec("plugins", r?.1.value()))
            .collect()
    }
}
