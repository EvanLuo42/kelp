use std::collections::BTreeMap;

use kelp_core::{ClNumber, ContentId, DepotPath, FileManifest, TreeId};
use redb::{ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::apply::{self, Origin};
use super::artifacts::NEXT_ARTIFACT;
use super::{
    Applied, CONSENSUS, MANIFESTS, META, MetaDb, Reader, SHELVES, append_log, enc, next_cl,
    submit,
};
use crate::{ArtifactContents, Command, LogIndex, Result, ShelfContents, Submission, Timestamp};

/// A change to the database, in the form a commit group agrees on.
///
/// Every write a server makes is one of these, so that the members of a
/// commit group, which apply the same proposals in the same order, end up
/// with the same database. Numbers such as the changelist of a submit are
/// assigned when a proposal is applied, not when it is made.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Proposal {
    /// Applies a command on behalf of `actor`.
    Command { actor: String, command: Command },
    /// Submits a changelist under the next number.
    Submit(Submission),
    /// Copies a stream into its parent; see [`MetaDb::copy_up`].
    CopyUp {
        from: DepotPath,
        author: String,
        description: String,
        at: Timestamp,
    },
    /// Shelves files under the next number or replacing a shelf.
    Shelve {
        replace: Option<ClNumber>,
        contents: ShelfContents,
    },
    /// Publishes an artifact under the next id.
    PublishArtifact(ArtifactContents),
    /// Stores file manifests.
    PutManifests(Vec<FileManifest>),
    /// Deletes manifests no longer in use and the tree nodes nothing
    /// reaches any more.
    Collect { dead_manifests: Vec<ContentId> },
    /// Records an attempt to send a webhook delivery; see
    /// [`MetaDb::record_delivery`].
    RecordDelivery {
        id: u64,
        outcome: std::result::Result<String, String>,
        at: Timestamp,
        retry: u64,
    },
    /// Queues a webhook delivery to be sent again.
    Redeliver { id: u64 },
    /// Applies several commands, each on behalf of its actor, in one
    /// transaction: all of them or none.
    Batch(Vec<(String, Command)>),
    /// Submits a changelist and records the files it moved, as `(from, to)`.
    SubmitMoves {
        submission: Submission,
        moves: Vec<(DepotPath, DepotPath)>,
    },
}

/// What applying a [`Proposal`] produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// A command was logged at `index`.
    Logged { index: LogIndex, applied: Applied },
    /// Files were shelved as changelist `cl`.
    Shelved { cl: ClNumber },
    /// Artifact `id` was published, logged at `index`.
    Published { index: LogIndex, id: u64 },
    /// Unused metadata was deleted, including `nodes` tree nodes.
    Collected { nodes: u64 },
    /// The proposal changed nothing that needs reporting.
    Done,
}

impl Outcome {
    /// Returns the log index and result of a logged command.
    ///
    /// # Panics
    ///
    /// Panics if the outcome is not [`Outcome::Logged`].
    pub fn logged(self) -> (LogIndex, Applied) {
        match self {
            Self::Logged { index, applied } => (index, applied),
            other => panic!("expected a logged command, got {other:?}"),
        }
    }

    /// Returns the log index, number and tree of a submitted changelist.
    ///
    /// # Panics
    ///
    /// Panics if the outcome is not a submit.
    pub fn submitted(self) -> (LogIndex, ClNumber, TreeId) {
        match self {
            Self::Logged {
                index,
                applied: Applied::Submitted { cl, root },
            } => (index, cl, root),
            other => panic!("expected a submit, got {other:?}"),
        }
    }
}

impl MetaDb {
    /// Applies `proposal` in one transaction.
    ///
    /// # Errors
    ///
    /// Returns the error the underlying operation fails with; nothing is
    /// written then.
    pub fn propose(&self, proposal: &Proposal) -> Result<Outcome> {
        self.apply_with_consensus(Some(proposal), &BTreeMap::new())
    }

    /// Applies `proposal`, if any, and stores the `consensus` entries, which
    /// a commit group uses to record how far it has applied its log.
    ///
    /// The entries are stored in the same transaction as the proposal's
    /// changes, so after a crash both or neither are present. If the
    /// proposal fails, the entries are stored anyway and its error returned:
    /// every member of the group fails the same way.
    ///
    /// # Errors
    ///
    /// Returns the error the proposal fails with, or [`Error::Db`] if the
    /// entries cannot be stored.
    ///
    /// [`Error::Db`]: crate::Error::Db
    pub fn apply_with_consensus(
        &self,
        proposal: Option<&Proposal>,
        consensus: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Outcome> {
        if let Some(Proposal::Collect { dead_manifests }) = proposal {
            // Marking reads committed state, so this runs in several
            // transactions; repeating it after a crash is harmless.
            self.delete_manifests(dead_manifests)?;
            let (live, through) = self.read()?.live_nodes()?;
            let nodes = self.sweep_nodes(&live, through)?;
            self.store_consensus(consensus)?;
            return Ok(Outcome::Collected { nodes });
        }
        let txn = self.db.begin_write()?;
        let outcome = match proposal {
            Some(p) => match apply_in(&txn, p) {
                Ok(outcome) => outcome,
                Err(e) => {
                    txn.abort()?;
                    self.store_consensus(consensus)?;
                    return Err(e);
                }
            },
            None => Outcome::Done,
        };
        write_consensus(&txn, consensus)?;
        txn.commit()?;
        Ok(outcome)
    }

    fn store_consensus(&self, consensus: &BTreeMap<String, Vec<u8>>) -> Result<()> {
        if consensus.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        write_consensus(&txn, consensus)?;
        txn.commit()?;
        Ok(())
    }
}

impl Reader {
    /// Returns the consensus entry `key` stored by
    /// [`MetaDb::apply_with_consensus`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`](crate::Error::Db) if the table cannot be read.
    pub fn consensus(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .txn()
            .open_table(CONSENSUS)?
            .get(key)?
            .map(|v| v.value().to_vec()))
    }
}

fn write_consensus(txn: &WriteTransaction, consensus: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let mut table = txn.open_table(CONSENSUS)?;
    for (key, value) in consensus {
        table.insert(key.as_str(), value.as_slice())?;
    }
    Ok(())
}

fn apply_in(txn: &WriteTransaction, proposal: &Proposal) -> Result<Outcome> {
    let logged = |cmd: &Command, actor: &str| -> Result<Outcome> {
        let applied = apply::apply(txn, cmd, Origin::Local, actor)?;
        let index = append_log(txn, cmd, actor, Origin::Local)?;
        Ok(Outcome::Logged { index, applied })
    };
    match proposal {
        Proposal::Command { actor, command } => logged(command, actor),
        Proposal::Submit(submission) => {
            if submission.server_side() {
                return Err(crate::Error::Invalid(
                    "submits need a workspace name".into(),
                ));
            }
            let cmd = Command::Submit {
                cl: next_cl(txn)?,
                submission: submission.clone(),
            };
            logged(&cmd, &submission.author)
        }
        Proposal::SubmitMoves { submission, moves } => {
            let outcome = apply_in(txn, &Proposal::Submit(submission.clone()))?;
            if let Outcome::Logged {
                applied: Applied::Submitted { cl, .. },
                ..
            } = &outcome
            {
                let record = Command::RecordMoves {
                    cl: *cl,
                    moves: moves.clone(),
                };
                logged(&record, &submission.author)?;
            }
            Ok(outcome)
        }
        Proposal::CopyUp {
            from,
            author,
            description,
            at,
        } => {
            let submission =
                submit::copy_up_submission(txn, from, author.clone(), description.clone(), *at)?;
            let cmd = Command::Submit {
                cl: next_cl(txn)?,
                submission,
            };
            logged(&cmd, author)
        }
        Proposal::Shelve { replace, contents } => {
            let cl = match replace {
                // The next number would otherwise make a new shelf.
                Some(cl) if txn.open_table(SHELVES)?.get(cl.0)?.is_none() => {
                    return Err(crate::Error::NotFound(format!("shelf {cl}")));
                }
                Some(cl) => *cl,
                None => next_cl(txn)?,
            };
            let cmd = Command::Shelve {
                cl,
                contents: contents.clone(),
            };
            logged(&cmd, &contents.owner)?;
            Ok(Outcome::Shelved { cl })
        }
        Proposal::PublishArtifact(contents) => {
            let id = txn
                .open_table(META)?
                .get(NEXT_ARTIFACT)?
                .map_or(1, |v| v.value());
            let cmd = Command::PublishArtifact {
                id,
                contents: contents.clone(),
            };
            let (index, _) = logged(&cmd, &contents.by)?.logged();
            Ok(Outcome::Published { index, id })
        }
        Proposal::PutManifests(manifests) => {
            let mut table = txn.open_table(MANIFESTS)?;
            for manifest in manifests {
                table.insert(manifest.content.0.0.as_slice(), enc(manifest).as_slice())?;
            }
            Ok(Outcome::Done)
        }
        Proposal::Collect { .. } => unreachable!("collected outside a transaction"),
        Proposal::RecordDelivery {
            id,
            outcome,
            at,
            retry,
        } => {
            super::governance::webhooks::record_delivery(txn, *id, outcome.clone(), *at, *retry)?;
            Ok(Outcome::Done)
        }
        Proposal::Batch(commands) => {
            for (actor, command) in commands {
                logged(command, actor)?;
            }
            Ok(Outcome::Done)
        }
        Proposal::Redeliver { id } => {
            super::governance::webhooks::redeliver(txn, *id)?;
            Ok(Outcome::Done)
        }
    }
}
