use kelp_core::{ClNumber, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use super::{LABELS, Reader, dec, enc, load_stream, name_key};
use crate::record::Label;
use crate::{Error, Result, Timestamp};

pub(super) fn set(
    txn: &WriteTransaction,
    name: &str,
    target: Option<&(DepotPath, ClNumber)>,
    description: &str,
    by: &str,
    at: Timestamp,
) -> Result<()> {
    super::apply::validate_name("label", name)?;
    let key = name_key(name);
    let mut labels = txn.open_table(LABELS)?;
    let Some((stream, cl)) = target else {
        return match labels.remove(key.as_str())? {
            Some(_) => Ok(()),
            None => Err(Error::NotFound(format!("label {name}"))),
        };
    };
    let record = load_stream(txn, stream)?;
    if *cl > record.head {
        return Err(Error::Invalid(format!("{stream} has no changelist {cl}")));
    }
    let label = Label {
        name: name.to_owned(),
        stream: stream.clone(),
        cl: *cl,
        description: description.to_owned(),
        owner: by.to_owned(),
        at,
    };
    labels.insert(key.as_str(), enc(&label).as_slice())?;
    Ok(())
}

impl Reader {
    /// Returns the label `name`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn label(&self, name: &str) -> Result<Option<Label>> {
        self.txn()
            .open_table(LABELS)?
            .get(name_key(name).as_str())?
            .map(|v| dec("labels", v.value()))
            .transpose()
    }

    /// Returns all labels, ordered by folded name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] or [`Error::CorruptRecord`] if the table cannot be read.
    pub fn labels(&self) -> Result<Vec<Label>> {
        self.txn()
            .open_table(LABELS)?
            .iter()?
            .map(|r| dec("labels", r?.1.value()))
            .collect()
    }
}
