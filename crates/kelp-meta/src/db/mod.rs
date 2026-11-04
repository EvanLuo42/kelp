//! The redb-backed metadata store and the state machine that applies commands.

mod accounts;
mod apply;
mod artifacts;
mod gc;
mod governance;
mod imports;
mod labels;
mod moves;
mod nodes;
mod obliterate;
mod proposal;
mod reader;
mod refold;
mod replica;
mod snapshot;
mod streams;
mod submit;

use std::path::Path;

use kelp_core::{ClNumber, DepotPath, FileManifest, TreeId};
use redb::{Database, ReadableTable, TableDefinition, WriteTransaction};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use accounts::SsoIdentity;
use apply::Origin;
pub use governance::webhooks;
pub use imports::RealPaths;
pub use nodes::ReadNodes;
pub use proposal::{Outcome, Proposal};
pub use reader::Reader;

use crate::record::{AuditEntry, Stream, User};
use crate::{Command, Error, Integration, LogIndex, Result, ShelfContents, Submission, Timestamp};

type Bytes = &'static [u8];

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const LOG: TableDefinition<u64, Bytes> = TableDefinition::new("log");
const NODES: TableDefinition<Bytes, Bytes> = TableDefinition::new("nodes");
const MANIFESTS: TableDefinition<Bytes, Bytes> = TableDefinition::new("manifests");
const USERS: TableDefinition<&str, Bytes> = TableDefinition::new("users");
const TOKENS: TableDefinition<Bytes, Bytes> = TableDefinition::new("tokens");
const GROUPS: TableDefinition<&str, Bytes> = TableDefinition::new("groups");
const SETTINGS: TableDefinition<&str, Bytes> = TableDefinition::new("settings");
const DEPOTS: TableDefinition<&str, Bytes> = TableDefinition::new("depots");
const TYPEMAPS: TableDefinition<&str, Bytes> = TableDefinition::new("typemaps");
const STREAMS: TableDefinition<&str, Bytes> = TableDefinition::new("streams");
const CHANGES: TableDefinition<u64, Bytes> = TableDefinition::new("changes");
const STREAM_CHANGES: TableDefinition<(&str, u64), ()> = TableDefinition::new("stream_changes");
const REVISIONS: TableDefinition<(&str, u32), Bytes> = TableDefinition::new("revisions");
const SHELVES: TableDefinition<u64, Bytes> = TableDefinition::new("shelves");
const LOCKS: TableDefinition<(&str, &str), Bytes> = TableDefinition::new("locks");
const INTEGRATIONS: TableDefinition<(&str, &str), u64> = TableDefinition::new("integrations");
const AUDIT: TableDefinition<u64, Bytes> = TableDefinition::new("audit");
const ARTIFACTS: TableDefinition<u64, Bytes> = TableDefinition::new("artifacts");
const ARTIFACT_INDEX: TableDefinition<(&str, &str, u64, u64), ()> =
    TableDefinition::new("artifact_index");
const ARTIFACT_RETENTION: TableDefinition<(&str, &str), u32> =
    TableDefinition::new("artifact_retention");
const REVIEWS: TableDefinition<u64, Bytes> = TableDefinition::new("reviews");
const WEBHOOKS: TableDefinition<&str, Bytes> = TableDefinition::new("webhooks");
const PLUGINS: TableDefinition<&str, Bytes> = TableDefinition::new("plugins");
const ISSUE_CHANGES: TableDefinition<(&str, u64), ()> = TableDefinition::new("issue_changes");
const DELIVERIES: TableDefinition<u64, Bytes> = TableDefinition::new("deliveries");
const IMPORTS: TableDefinition<&str, Bytes> = TableDefinition::new("imports");
const IMPORTERS: TableDefinition<(&str, &str), ()> = TableDefinition::new("importers");
const VIEWS: TableDefinition<(&str, u64), Bytes> = TableDefinition::new("views");
const STREAM_ROOTS: TableDefinition<(&str, u64), Bytes> = TableDefinition::new("stream_roots");
const PENDING_DELIVERIES: TableDefinition<u64, ()> = TableDefinition::new("pending_deliveries");
const CONSENSUS: TableDefinition<&str, Bytes> = TableDefinition::new("consensus");
const SSO_SUBJECTS: TableDefinition<(&str, &str), &str> = TableDefinition::new("sso_subjects");
const SSO_LINKS: TableDefinition<&str, Bytes> = TableDefinition::new("sso_links");
const SSO_EXPECTED: TableDefinition<&str, &str> = TableDefinition::new("sso_expected");
const ALIASES: TableDefinition<&str, &str> = TableDefinition::new("aliases");
const VIEW_IMPORTS: TableDefinition<(&str, u64), Bytes> = TableDefinition::new("view_imports");
const DISABLED: TableDefinition<&str, ()> = TableDefinition::new("disabled");
const LABELS: TableDefinition<&str, Bytes> = TableDefinition::new("labels");
const PROFILES: TableDefinition<&str, Bytes> = TableDefinition::new("profiles");
const EMAIL_OWNERS: TableDefinition<&str, &str> = TableDefinition::new("email_owners");
const MOVES: TableDefinition<(&str, u64), Bytes> = TableDefinition::new("moves");

const LAST_INDEX: &str = "last_index";
const FORMAT: &str = "format_version";

/// The version of the database format this build reads and writes.
///
/// Every change to a table's layout or to how records, commands or tree
/// nodes are encoded must increase it and add a step to [`MIGRATIONS`].
/// Logged commands are kept as they were encoded, so a change to
/// [`Command`] also needs a migration that re-encodes the log; edges only
/// replicate from a commit server with the same format.
pub const FORMAT_VERSION: u64 = 10;

/// Converts a database from one format version to the next.
type Migration = fn(&WriteTransaction) -> Result<()>;

/// The migration from version `i + 1` to `i + 2` is at index `i`.
const MIGRATIONS: &[Migration] = &[
    // 2 adds the import tables, which opening the database creates.
    |_| Ok(()),
    // 3 adds the consensus table, likewise.
    |_| Ok(()),
    // 4 adds account links and aliases; users that single sign-on created
    // before then are expected to link on their next sign-on.
    accounts::expect_sso_created_users,
    // 5 keeps the imports each view was composed with.
    imports::record_view_imports,
    // 6 adds the table of disabled users, which opening the database creates.
    |_| Ok(()),
    // 7 adds labels, likewise.
    |_| Ok(()),
    // 8 adds the record of moved files, likewise.
    |_| Ok(()),
    // 9 adds user profiles and the owners of their addresses, likewise.
    |_| Ok(()),
    // 10 folds names one character at a time, so keys with a final sigma
    // and similar letters change.
    refold::refold,
];
const NEXT_CL: &str = "next_cl";
const PROTECTIONS: &str = "protections";
const REVIEW_RULES: &str = "review_rules";

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).expect("record serialization is infallible")
}

fn dec<T: DeserializeOwned>(table: &'static str, bytes: &[u8]) -> Result<T> {
    postcard::from_bytes(bytes).map_err(|_| Error::CorruptRecord(table))
}

/// Returns the case-folded key for a user, group or depot name.
pub fn name_key(name: &str) -> String {
    name.to_lowercase()
}

/// Returns the path of `path` relative to `stream`, such as `a/b` for
/// `//depot/stream/a/b` in `//depot/stream`.
///
/// Returns `None` if `path` is `stream` itself or lies outside it.
pub fn stream_relative(stream: &DepotPath, path: &DepotPath) -> Option<String> {
    if !path.starts_with(stream) || path == stream {
        return None;
    }
    Some(
        path.segments()
            .skip(stream.segments().count())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// The outcome of applying a [`Command`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Applied {
    /// The command was applied and produced no further result.
    Done,
    /// A changelist was submitted.
    Submitted {
        /// The number of the new changelist.
        cl: ClNumber,
        /// The stream's file tree after the submit.
        root: TreeId,
    },
}

/// The metadata database: a command log plus the state derived from it.
///
/// Each write applies a [`Command`] and appends it to the log in a single
/// transaction. Reads go through a consistent snapshot obtained from
/// [`read`](Self::read).
pub struct MetaDb {
    db: Database,
}

impl MetaDb {
    /// Opens the database at `path`, creating it and its tables if needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the file cannot be opened or initialized.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        migrate(&txn, path)?;
        txn.open_table(LOG)?;
        txn.open_table(NODES)?;
        txn.open_table(MANIFESTS)?;
        txn.open_table(USERS)?;
        txn.open_table(TOKENS)?;
        txn.open_table(GROUPS)?;
        txn.open_table(SETTINGS)?;
        txn.open_table(DEPOTS)?;
        txn.open_table(TYPEMAPS)?;
        txn.open_table(STREAMS)?;
        txn.open_table(CHANGES)?;
        txn.open_table(STREAM_CHANGES)?;
        txn.open_table(REVISIONS)?;
        txn.open_table(SHELVES)?;
        txn.open_table(LOCKS)?;
        txn.open_table(INTEGRATIONS)?;
        txn.open_table(AUDIT)?;
        txn.open_table(ARTIFACTS)?;
        txn.open_table(ARTIFACT_INDEX)?;
        txn.open_table(ARTIFACT_RETENTION)?;
        txn.open_table(REVIEWS)?;
        txn.open_table(WEBHOOKS)?;
        txn.open_table(PLUGINS)?;
        txn.open_table(IMPORTS)?;
        txn.open_table(IMPORTERS)?;
        txn.open_table(VIEWS)?;
        txn.open_table(STREAM_ROOTS)?;
        txn.open_table(ISSUE_CHANGES)?;
        txn.open_table(DELIVERIES)?;
        txn.open_table(PENDING_DELIVERIES)?;
        txn.open_table(CONSENSUS)?;
        txn.open_table(SSO_SUBJECTS)?;
        txn.open_table(SSO_LINKS)?;
        txn.open_table(SSO_EXPECTED)?;
        txn.open_table(ALIASES)?;
        txn.open_table(VIEW_IMPORTS)?;
        txn.open_table(DISABLED)?;
        txn.open_table(LABELS)?;
        txn.open_table(MOVES)?;
        txn.open_table(PROFILES)?;
        txn.open_table(EMAIL_OWNERS)?;
        txn.commit()?;
        Ok(Self { db })
    }

    /// Applies `cmd` and appends it to the log atomically.
    ///
    /// Returns the log index of the command and what applying it produced. On
    /// error nothing is written.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] for a malformed command,
    /// [`Error::AlreadyExists`] or [`Error::NotFound`] when the target object is
    /// present or missing contrary to what the command requires,
    /// [`Error::Locked`] if a lock is held by someone else, the errors of
    /// [`submit`](Self::submit) for [`Command::Submit`], and [`Error::Db`] if the
    /// transaction fails.
    pub fn commit(&self, cmd: impl Into<Command>) -> Result<(LogIndex, Applied)> {
        self.commit_by("system", cmd)
    }

    /// Applies `cmd` on behalf of `actor`, who is recorded in the audit log.
    ///
    /// # Errors
    ///
    /// As for [`commit`](Self::commit).
    pub fn commit_by(&self, actor: &str, cmd: impl Into<Command>) -> Result<(LogIndex, Applied)> {
        let proposal = Proposal::Command {
            actor: actor.to_owned(),
            command: cmd.into(),
        };
        Ok(self.propose(&proposal)?.logged())
    }

    /// Submits a changelist and returns its log index, number and new file tree.
    ///
    /// The changelist number is assigned inside the write transaction, so
    /// concurrent submits are serialized.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream does not exist,
    /// [`Error::Invalid`] if the changes are empty, name a path twice or lie
    /// outside the stream, or the integration is not valid for the stream,
    /// [`Error::Conflict`] if a change is based on content that is no longer
    /// current or an exclusive file is not locked by the submitting workspace,
    /// [`Error::MissingContent`] if a manifest has not been stored with
    /// [`put_manifest`](Self::put_manifest), [`Error::PathConflict`] if a file
    /// would replace a directory or vice versa, and [`Error::Db`] if the
    /// transaction fails.
    pub fn submit(&self, submission: Submission) -> Result<(LogIndex, ClNumber, TreeId)> {
        Ok(self.propose(&Proposal::Submit(submission))?.submitted())
    }

    /// Copies the changes of `from` into its parent stream as one changelist.
    ///
    /// `from` must have merged down every change of its parent first, so the
    /// copy cannot lose changes made in the parent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Invalid`] if `from` cannot copy up or has nothing to
    /// copy, [`Error::Conflict`] if it has not merged down its parent's latest
    /// changes or a changed exclusive file is locked, and the errors of
    /// [`submit`](Self::submit).
    pub fn copy_up(
        &self,
        from: &DepotPath,
        author: String,
        description: String,
        at: Timestamp,
    ) -> Result<(LogIndex, ClNumber, TreeId)> {
        let proposal = Proposal::CopyUp {
            from: from.clone(),
            author,
            description,
            at,
        };
        Ok(self.propose(&proposal)?.submitted())
    }

    /// Shelves `contents`, as a new shelf or replacing shelf `replace`, and
    /// returns the shelf's changelist number.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if the stream or the shelf to replace does
    /// not exist, [`Error::Invalid`] if the files are empty or lie outside the
    /// stream, [`Error::MissingContent`] if a manifest has not been stored, and
    /// [`Error::Db`] if the transaction fails.
    pub fn shelve(&self, replace: Option<ClNumber>, contents: ShelfContents) -> Result<ClNumber> {
        match self.propose(&Proposal::Shelve { replace, contents })? {
            Outcome::Shelved { cl } => Ok(cl),
            other => unreachable!("shelving produced {other:?}"),
        }
    }

    /// Stores a file manifest, keyed by its content id.
    ///
    /// Manifests are content-addressed data rather than state, so they are not
    /// logged; replicas fetch them on demand.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the transaction fails.
    pub fn put_manifest(&self, manifest: &FileManifest) -> Result<()> {
        self.put_manifests(std::slice::from_ref(manifest))
    }

    /// Stores several file manifests in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the transaction fails.
    pub fn put_manifests(&self, manifests: &[FileManifest]) -> Result<()> {
        self.propose(&Proposal::PutManifests(manifests.to_vec()))?;
        Ok(())
    }

    /// Returns a consistent read snapshot of the current state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Db`] if the read transaction cannot be started.
    pub fn read(&self) -> Result<Reader> {
        Ok(Reader::new(self.db.begin_read()?))
    }
}

/// Returns the next changelist number, shared by submits and shelves.
/// Brings the database to [`FORMAT_VERSION`], stamping a new database with
/// it.
fn migrate(txn: &WriteTransaction, path: &Path) -> Result<()> {
    let mut meta = txn.open_table(META)?;
    let found = meta.get(FORMAT)?.map(|v| v.value());
    let fresh = meta.iter()?.next().is_none();
    let version = match found {
        Some(v) => v,
        None if fresh => FORMAT_VERSION,
        None => {
            return Err(Error::Format(format!(
                "{} was created by a development build of Kelp without format versioning; \
                 create a new data directory",
                path.display()
            )));
        }
    };
    if version > FORMAT_VERSION {
        return Err(Error::Format(format!(
            "{} has format {version}, but this build only understands up to {FORMAT_VERSION}; \
             upgrade Kelp",
            path.display()
        )));
    }
    meta.insert(FORMAT, version)?;
    drop(meta);
    for (i, step) in MIGRATIONS.iter().enumerate().skip(version as usize - 1) {
        step(txn)?;
        txn.open_table(META)?.insert(FORMAT, i as u64 + 2)?;
    }
    Ok(())
}

/// Returns the stream at `path`, or [`Error::NotFound`].
fn load_stream(txn: &WriteTransaction, path: &DepotPath) -> Result<Stream> {
    match txn.open_table(STREAMS)?.get(path.key())? {
        Some(v) => dec("streams", v.value()),
        None => Err(Error::NotFound(format!("stream {path}"))),
    }
}

/// Returns the user named `name`, or [`Error::NotFound`].
fn load_user(txn: &WriteTransaction, name: &str) -> Result<User> {
    match txn.open_table(USERS)?.get(name_key(name).as_str())? {
        Some(v) => dec("users", v.value()),
        None => Err(Error::NotFound(format!("user {name}"))),
    }
}

/// Stores `user` under its name.
fn save_user(txn: &WriteTransaction, user: &User) -> Result<()> {
    txn.open_table(USERS)?
        .insert(name_key(&user.name).as_str(), enc(user).as_slice())?;
    Ok(())
}

fn next_cl(txn: &WriteTransaction) -> Result<ClNumber> {
    Ok(ClNumber(
        txn.open_table(META)?.get(NEXT_CL)?.map_or(1, |v| v.value()),
    ))
}

/// Appends `cmd` to the log and the audit chain. A command proposed on this
/// server also queues its event for webhooks; replicas never deliver.
fn append_log(
    txn: &WriteTransaction,
    cmd: &Command,
    actor: &str,
    origin: Origin,
) -> Result<LogIndex> {
    let mut meta = txn.open_table(META)?;
    let index = meta.get(LAST_INDEX)?.map_or(0, |v| v.value()) + 1;
    meta.insert(LAST_INDEX, index)?;
    let bytes = enc(cmd);
    txn.open_table(LOG)?.insert(index, bytes.as_slice())?;
    let mut audit = txn.open_table(AUDIT)?;
    let prev = match audit.get(index - 1)? {
        Some(v) => dec::<AuditEntry>("audit", v.value())?.hash,
        None if index == 1 => kelp_core::Hash([0; 32]),
        None => return Err(Error::AuditMismatch(index - 1)),
    };
    let entry = AuditEntry {
        index,
        actor: actor.to_owned(),
        summary: cmd.summary(),
        prev,
        hash: audit_hash(&prev, index, actor, &bytes),
    };
    audit.insert(index, enc(&entry).as_slice())?;
    drop((audit, meta));
    if origin == Origin::Local {
        governance::webhooks::enqueue(txn, index, actor, cmd)?;
    }
    Ok(LogIndex(index))
}

/// Chains an audit entry to its predecessor.
fn audit_hash(prev: &kelp_core::Hash, index: u64, actor: &str, command: &[u8]) -> kelp_core::Hash {
    let mut h = blake3::Hasher::new();
    h.update(&prev.0);
    h.update(&index.to_le_bytes());
    h.update(&(actor.len() as u64).to_le_bytes());
    h.update(actor.as_bytes());
    h.update(command);
    kelp_core::Hash(*h.finalize().as_bytes())
}

/// Returns the integration watermark of `target` from `source`.
fn integration(
    txn: &WriteTransaction,
    target: &DepotPath,
    source: &DepotPath,
) -> Result<Option<ClNumber>> {
    Ok(txn
        .open_table(INTEGRATIONS)?
        .get((target.key(), source.key()))?
        .map(|v| ClNumber(v.value())))
}

fn set_integration(
    txn: &WriteTransaction,
    target: &DepotPath,
    integration: &Integration,
) -> Result<()> {
    txn.open_table(INTEGRATIONS)?.insert(
        (target.key(), integration.source.key()),
        integration.through.0,
    )?;
    Ok(())
}
