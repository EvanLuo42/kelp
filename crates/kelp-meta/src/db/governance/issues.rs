use std::collections::{BTreeMap, BTreeSet, HashMap};

use kelp_core::issue::parse_key;
use kelp_core::{ClNumber, DepotPath};
use redb::{ReadableTable, WriteTransaction};

use crate::db::{CHANGES, ISSUE_CHANGES, Reader, dec, enc};
use crate::record::Changelist;
use crate::{Error, Governance, Result};

/// Returns `issues` and `fixes` as canonical keys without duplicates; fixed
/// issues are linked too.
pub(in crate::db) fn canonical_links(
    issues: &[String],
    fixes: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut linked: Vec<String> = Vec::new();
    let mut fixed: Vec<String> = Vec::new();
    for key in issues.iter().chain(fixes) {
        let key = canonical(key)?;
        if !linked.contains(&key) {
            linked.push(key);
        }
    }
    for key in fixes {
        let key = canonical(key)?;
        if !fixed.contains(&key) {
            fixed.push(key);
        }
    }
    Ok((linked, fixed))
}

/// Indexes changelist `cl` under each of the canonical keys `issues`.
pub(in crate::db) fn link(txn: &WriteTransaction, cl: ClNumber, issues: &[String]) -> Result<()> {
    let mut index = txn.open_table(ISSUE_CHANGES)?;
    for key in issues {
        index.insert((key.as_str(), cl.0), ())?;
    }
    Ok(())
}

pub(in crate::db) fn apply(txn: &WriteTransaction, cmd: &Governance) -> Result<()> {
    let Governance::LinkIssues { cl, add, remove } = cmd else {
        unreachable!("not an issue command");
    };
    let mut changes = txn.open_table(CHANGES)?;
    let mut record: Changelist = match changes.get(cl.0)? {
        Some(v) => dec("changes", v.value())?,
        None => return Err(Error::NotFound(format!("change {cl}"))),
    };
    let mut index = txn.open_table(ISSUE_CHANGES)?;
    for key in remove {
        let key = canonical(key)?;
        index.remove((key.as_str(), cl.0))?;
        record.issues.retain(|k| *k != key);
        record.fixes.retain(|k| *k != key);
    }
    for key in add {
        let key = canonical(key)?;
        index.insert((key.as_str(), cl.0), ())?;
        if !record.issues.contains(&key) {
            record.issues.push(key);
        }
    }
    changes.insert(cl.0, enc(&record).as_slice())?;
    Ok(())
}

fn canonical(key: &str) -> Result<String> {
    parse_key(key)
        .ok_or_else(|| Error::Invalid(format!("{key:?} is not an issue key like PROJ-123")))
}

/// Which changes of which streams a query has collected.
#[derive(Default)]
struct Containment {
    changes: BTreeSet<u64>,
    /// The highest changelist each stream was walked up to.
    walked: HashMap<String, u64>,
}

impl Reader {
    /// Returns the changelists linked to issue `key`, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if `key` is not an issue key, or
    /// [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn issue_changes(&self, key: &str) -> Result<Vec<Changelist>> {
        let key = canonical(key)?;
        let index = self.txn().open_table(ISSUE_CHANGES)?;
        let mut out = Vec::new();
        for row in index.range((key.as_str(), 0u64)..=(key.as_str(), u64::MAX))? {
            let cl = row?.0.value().1;
            if let Some(change) = self.changelist(ClNumber(cl))? {
                out.push(change);
            }
        }
        Ok(out)
    }

    /// Returns the changelists whose changes `stream` contains as of its
    /// changelist `at`: its own, those merged or copied into it, and its
    /// parent's up to when it was created, recursively.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream does not exist, or
    /// [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn contained_changes(
        &self,
        stream: &DepotPath,
        at: ClNumber,
    ) -> Result<BTreeSet<ClNumber>> {
        let mut found = Containment::default();
        self.collect(stream, at.0, &mut found)?;
        Ok(found.changes.into_iter().map(ClNumber).collect())
    }

    fn collect(&self, stream: &DepotPath, at: u64, found: &mut Containment) -> Result<()> {
        let walked = found.walked.get(stream.key()).copied().unwrap_or(0);
        if at <= walked && found.walked.contains_key(stream.key()) {
            return Ok(());
        }
        found.walked.insert(stream.key().to_owned(), at);
        let record = self
            .stream(stream)?
            .ok_or_else(|| Error::NotFound(format!("stream {stream}")))?;
        let own: Vec<u64> = {
            let index = self.txn().open_table(crate::db::STREAM_CHANGES)?;
            index
                .range((stream.key(), walked)..=(stream.key(), at))?
                .map(|r| Ok(r?.0.value().1))
                .collect::<Result<_>>()?
        };
        for cl in own {
            found.changes.insert(cl);
            if let Some(integration) = self.changelist(ClNumber(cl))?.and_then(|c| c.integration) {
                self.collect(&integration.source, integration.through.0, found)?;
            }
        }
        if let Some(parent) = &record.parent
            && record.created_from.0 > 0
        {
            self.collect(parent, record.created_from.0, found)?;
        }
        Ok(())
    }

    /// Returns the issues linked to changelists that `stream` contains as of
    /// `at` but not as of `since`, with those changelists.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream does not exist, or
    /// [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn issues_in(
        &self,
        stream: &DepotPath,
        at: ClNumber,
        since: Option<ClNumber>,
    ) -> Result<BTreeMap<String, Vec<ClNumber>>> {
        let mut changes = self.contained_changes(stream, at)?;
        if let Some(since) = since {
            let before = self.contained_changes(stream, since)?;
            changes.retain(|c| !before.contains(c));
        }
        let mut issues: BTreeMap<String, Vec<ClNumber>> = BTreeMap::new();
        for cl in changes {
            if let Some(change) = self.changelist(cl)? {
                for key in change.issues {
                    issues.entry(key).or_default().push(cl);
                }
            }
        }
        Ok(issues)
    }

    /// Returns the streams that contain a changelist linked to issue `key`,
    /// with the changelist of each through which it arrived there: one of
    /// the linked changelists, the merge or copy that brought one in, or
    /// `ClNumber(0)` if the stream was created with it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if `key` is not an issue key, or
    /// [`Error::Db`] or [`Error::CorruptRecord`] if a table cannot be read.
    pub fn issue_reach(&self, key: &str) -> Result<Vec<(DepotPath, ClNumber)>> {
        let linked: BTreeSet<ClNumber> = self
            .issue_changes(key)?
            .into_iter()
            .map(|c| c.number)
            .collect();
        if linked.is_empty() {
            return Ok(Vec::new());
        }
        let mut reached = Vec::new();
        for stream in self.streams()? {
            let has = |found: &BTreeSet<ClNumber>| found.iter().any(|c| linked.contains(c));
            if !has(&self.contained_changes(&stream.path, stream.head)?) {
                continue;
            }
            let arrival = match &stream.parent {
                Some(p)
                    if stream.created_from.0 > 0
                        && has(&self.contained_changes(p, stream.created_from)?) =>
                {
                    ClNumber(0)
                }
                _ => self.arrival(&stream.path, &linked)?,
            };
            reached.push((stream.path, arrival));
        }
        Ok(reached)
    }

    /// Returns the first changelist of `stream` after which it contains one
    /// of `linked`.
    fn arrival(&self, stream: &DepotPath, linked: &BTreeSet<ClNumber>) -> Result<ClNumber> {
        let own: Vec<u64> = {
            let index = self.txn().open_table(crate::db::STREAM_CHANGES)?;
            index
                .range((stream.key(), 0u64)..=(stream.key(), u64::MAX))?
                .map(|r| Ok(r?.0.value().1))
                .collect::<Result<_>>()?
        };
        for cl in own {
            if linked.contains(&ClNumber(cl)) {
                return Ok(ClNumber(cl));
            }
            if let Some(integration) = self.changelist(ClNumber(cl))?.and_then(|c| c.integration)
                && self
                    .contained_changes(&integration.source, integration.through)?
                    .iter()
                    .any(|c| linked.contains(c))
            {
                return Ok(ClNumber(cl));
            }
        }
        Ok(ClNumber(0))
    }
}
