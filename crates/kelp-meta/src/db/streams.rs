use kelp_core::DepotPath;
use redb::{ReadableTable, WriteTransaction};

use super::{IMPORTERS, IMPORTS, INTEGRATIONS, LABELS, LOCKS, SHELVES, STREAMS, dec, load_stream};
use crate::record::{Label, Shelf, Stream};
use crate::{Error, Result, StreamKind};

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
    Ok(())
}
