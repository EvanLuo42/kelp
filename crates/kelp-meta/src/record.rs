//! Records stored in the metadata database.

use kelp_core::{ClNumber, ContentId, DepotPath, TreeId};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactContents, FileChange, Integration, LockDomain, ShelfContents, StreamKind, Timestamp,
};

/// A user account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// The name as originally spelled.
    pub name: String,
    /// The password hash as an argon2id PHC string, or `None` for a service
    /// account.
    pub password_hash: Option<String>,
    /// Whether the user has administrative rights.
    pub admin: bool,
    /// Whether the user is an edge server's service account, which may read
    /// the command log.
    pub replica: bool,
    /// The base32 TOTP secret, if the user logs in with a second factor.
    pub totp: Option<String>,
    /// When the user was created.
    pub created_at: Timestamp,
}

/// An access token, stored under the hash of its secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    /// The user the token authenticates as.
    pub user: String,
    /// When the token stops being valid.
    pub expires_at: Timestamp,
}

/// A depot, the top-level namespace for streams.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Depot {
    /// The name as originally spelled.
    pub name: String,
    /// When the depot was created.
    pub created_at: Timestamp,
}

/// A stream, a branch within a depot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// The path of the form `//depot/stream`.
    pub path: DepotPath,
    /// The role of the stream.
    pub kind: StreamKind,
    /// The stream this one branches from, if any.
    pub parent: Option<DepotPath>,
    /// The latest changelist submitted to this stream, or `ClNumber(0)` if none.
    pub head: ClNumber,
    /// The file tree at [`head`](Self::head).
    pub root: TreeId,
    /// The file tree the stream started with: its parent's tree when it was
    /// created, or the empty tree.
    pub base_root: TreeId,
    /// The family of streams this stream shares exclusive locks with.
    pub lock_domain: LockDomain,
    /// The parent's head when this stream was created from it; the stream
    /// contains the parent's changes up to it.
    pub created_from: ClNumber,
    /// When the stream was created.
    pub created_at: Timestamp,
}

/// A submitted changelist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changelist {
    /// The changelist number.
    pub number: ClNumber,
    /// The stream the changelist was submitted to.
    pub stream: DepotPath,
    /// The name of the submitting user.
    pub author: String,
    /// The changelist description.
    pub description: String,
    /// When the changelist was submitted.
    pub submitted_at: Timestamp,
    /// The stream's file tree after the submit.
    pub root: TreeId,
    /// The file changes, in submitted order.
    pub changes: Vec<FileChange>,
    /// The merge or copy this changelist completed, if any.
    pub integration: Option<Integration>,
    /// The issue keys the changelist is linked to; see [`Submission::issues`].
    ///
    /// [`Submission::issues`]: crate::Submission::issues
    pub issues: Vec<String>,
    /// The linked issues it fixes.
    pub fixes: Vec<String>,
}

/// The typemap of a depot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypemapRecord {
    /// The typemap source; see [`kelp_core::typemap`].
    pub source: String,
    /// The administrator who set it.
    pub set_by: String,
    /// When it was set.
    pub set_at: Timestamp,
}

/// A named set of users.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// The name as originally spelled.
    pub name: String,
    /// The member user names.
    pub members: Vec<String>,
}

/// A shelved changelist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    /// The shelf's changelist number.
    pub number: ClNumber,
    /// What is shelved.
    pub contents: ShelfContents,
}

/// The kind of change recorded by a [`Revision`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevAction {
    /// The file was created.
    Add,
    /// The file content was replaced.
    Edit,
    /// The file was removed.
    Delete,
}

/// One revision in the history of a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    /// The file path.
    pub path: DepotPath,
    /// The revision number, starting at 1 for each path.
    pub rev: u32,
    /// The changelist that created this revision.
    pub cl: ClNumber,
    /// The kind of change.
    pub action: RevAction,
    /// The file content, or `None` for a deletion.
    pub content: Option<ContentId>,
    /// The file size in bytes, or 0 for a deletion.
    pub size: u64,
    /// Whether the content was discarded under the typemap's `retain` limit.
    pub purged: bool,
}

/// One link of the audit chain kept alongside the command log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// The log index of the command.
    pub index: u64,
    /// The user on whose behalf the command was applied.
    pub actor: String,
    /// A description of the command without secrets.
    pub summary: String,
    /// The hash of the previous entry, or zero for the first.
    pub prev: kelp_core::Hash,
    /// The hash of this entry: `prev`, `index`, `actor` and the encoded command.
    pub hash: kelp_core::Hash,
}

/// A published artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: u64,
    pub stream: DepotPath,
    pub cl: ClNumber,
    pub name: String,
    pub tags: Vec<(String, String)>,
    /// The artifact's file tree.
    pub root: TreeId,
    pub files: u64,
    pub bytes: u64,
    pub by: String,
    pub at: Timestamp,
    pub pinned: bool,
}

impl Artifact {
    /// Returns `true` if every tag in `wanted` is present with the same value.
    pub fn has_tags(&self, wanted: &[(String, String)]) -> bool {
        wanted.iter().all(|w| self.tags.contains(w))
    }

    pub(crate) fn from_contents(id: u64, root: TreeId, c: &ArtifactContents) -> Self {
        Self {
            id,
            stream: c.stream.clone(),
            cl: c.cl,
            name: c.name.clone(),
            tags: c.tags.clone(),
            root,
            files: c.files.len() as u64,
            bytes: c.files.iter().map(|(_, d)| d.size).sum(),
            by: c.by.clone(),
            at: c.at,
            pinned: false,
        }
    }
}

/// A registered plugin version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecord {
    pub name: String,
    pub version: String,
    /// The component file.
    pub content: kelp_core::ContentId,
    pub size: u64,
    /// The worlds the component implements, such as `merger`.
    pub worlds: Vec<String>,
    pub by: String,
    pub at: Timestamp,
}

/// An HTTP endpoint that receives events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Webhook {
    pub name: String,
    pub url: String,
    pub secret: String,
    /// Event kind prefixes, such as `submit` or `review`; empty for all.
    pub events: Vec<String>,
    /// Path patterns as in the protections table; empty for all.
    pub paths: Vec<String>,
}

/// One event queued for, or sent to, one webhook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub id: u64,
    pub webhook: String,
    /// The event kind, such as `submit`.
    pub kind: String,
    /// The JSON body.
    pub body: Vec<u8>,
    pub attempts: u32,
    /// When the next attempt is due, if the delivery is pending.
    pub next_at: Option<Timestamp>,
    pub delivered: bool,
    /// The HTTP status or error of the last attempt.
    pub last_result: Option<String>,
}

/// The submit gate's record of a shelf under review: who approved or
/// rejected which version, and what automated checks reported. Discussion
/// belongs in other tools.
///
/// Each time the shelf is shelved again the review gets a new version.
/// Votes and status reports apply to the version they were made on, so a
/// change after approval needs approving again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// The number of the reviewed shelf.
    pub shelf: ClNumber,
    pub stream: DepotPath,
    pub author: String,
    pub description: String,
    /// Users asked to review.
    pub reviewers: Vec<String>,
    /// The shelved files of each version, oldest first; version `n` is at
    /// index `n - 1`.
    pub versions: Vec<ReviewVersion>,
    /// The latest vote of each user.
    pub votes: Vec<Vote>,
    pub statuses: Vec<StatusReport>,
    pub state: ReviewState,
    pub opened_at: Timestamp,
}

impl Review {
    /// Returns the latest version number.
    pub fn version(&self) -> u32 {
        self.versions.len() as u32
    }

    /// Returns the files of the latest version.
    pub fn files(&self) -> &[crate::FileChange] {
        &self.versions.last().expect("a review has a version").files
    }

    /// Returns the latest report of `context` for the latest version.
    pub fn status(&self, context: &str) -> Option<&StatusReport> {
        self.statuses
            .iter()
            .rev()
            .find(|s| s.version == self.version() && s.context == context)
    }
}

/// One version of a [`Review`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVersion {
    pub files: Vec<crate::FileChange>,
    pub description: String,
    pub at: Timestamp,
}

/// A user's verdict on one version of a review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub user: String,
    pub version: u32,
    /// Whether the user approves; `false` rejects.
    pub approve: bool,
    pub at: Timestamp,
}

/// The outcome of an automated check, such as a CI build, on one version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// What was checked, such as `build`.
    pub context: String,
    pub version: u32,
    pub state: CheckState,
    /// Where to find details.
    pub url: Option<String>,
    pub by: String,
    pub at: Timestamp,
}

/// The state of a [`StatusReport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
}

/// Where a [`Review`] is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    Open,
    /// The reviewed change was submitted as this changelist.
    Submitted(ClNumber),
    /// The review was closed without submitting.
    Closed,
}

/// A directory of another stream that a stream shows as its own, such as
/// an engine's sources inside a game.
///
/// The importing stream's own tree never holds files below [`to`]; the
/// server composes what clients see. Read-only imports may be pinned to a
/// changelist; writable ones follow the source, and changes below [`to`]
/// are submitted to the source stream in the same changelist.
///
/// [`to`]: Import::to
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// The stream imported from.
    pub source: DepotPath,
    /// The directory of `source` imported, relative to it; empty for all of
    /// it.
    pub from: String,
    /// Where it appears in the importing stream, relative to it.
    pub to: String,
    /// The changelist of `source` shown, or `None` to follow its head.
    pub at: Option<ClNumber>,
    pub writable: bool,
}

impl std::fmt::Display for Import {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = match self.from.is_empty() {
            true => self.source.to_string(),
            false => format!("{}/{}", self.source, self.from),
        };
        write!(f, "{from}/... as {}/...", self.to)?;
        if let Some(cl) = self.at {
            write!(f, " at {cl}")?;
        }
        if self.writable {
            f.write_str(" (writable)")?;
        }
        Ok(())
    }
}

/// A name for a stream as of a changelist, such as a release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// The name as originally spelled.
    pub name: String,
    pub stream: DepotPath,
    pub cl: ClNumber,
    pub description: String,
    /// The user who last set the label, who may move or delete it.
    pub owner: String,
    pub at: Timestamp,
}
