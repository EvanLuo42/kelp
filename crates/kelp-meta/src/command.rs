//! Commands that make up the replicated metadata log.

use kelp_core::{ClNumber, ContentId, DepotPath, Hash};
use serde::{Deserialize, Serialize};

use crate::governance::Governance;
use crate::{LockDomain, LockHolder};

/// Wall-clock time in milliseconds since the Unix epoch.
///
/// Timestamps are chosen by the proposer and stored in the command, so
/// replaying the log is deterministic.
pub type Timestamp = u64;

/// The content and attributes of a file revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileData {
    /// The hash of the file content.
    pub content: ContentId,
    /// The file size in bytes.
    pub size: u64,
    /// Whether the file has the executable bit set.
    pub executable: bool,
}

/// The change made to one file in a submit or shelf.
///
/// Concurrency is checked against the `base` content the client based its
/// change on rather than a revision number, so the check works the same on
/// branches that share history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAction {
    /// Creates a file that must not exist yet.
    Add {
        /// The new revision.
        data: FileData,
    },
    /// Replaces a file whose current content must be `base`.
    Edit {
        /// The content the change was based on.
        base: ContentId,
        /// The new revision.
        data: FileData,
    },
    /// Removes a file whose current content must be `base`.
    Delete {
        /// The content the change was based on.
        base: ContentId,
    },
}

impl FileAction {
    /// Returns the new revision, or `None` for a deletion.
    pub fn data(&self) -> Option<FileData> {
        match self {
            Self::Add { data } | Self::Edit { data, .. } => Some(*data),
            Self::Delete { .. } => None,
        }
    }
}

/// A [`FileAction`] applied to a depot path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The file being changed.
    pub path: DepotPath,
    /// The change to make.
    pub action: FileAction,
}

/// The role of a stream in the branching model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    /// A root stream with no parent.
    Mainline,
    /// A branch that routinely merges down from its parent and copies up after merging down.
    Development,
    /// A stabilization branch that only merges down from its parent.
    Release,
    /// A filtered view of its parent that stores no data of its own.
    Virtual,
    /// A lightweight, short-lived branch that records only its differences from its parent.
    Task,
}

impl StreamKind {
    /// Returns `true` if streams of this kind need a parent.
    pub fn has_parent(self) -> bool {
        self != Self::Mainline
    }

    /// Returns `true` if streams of this kind share their parent's lock domain.
    pub fn shares_parent_locks(self) -> bool {
        matches!(self, Self::Development | Self::Virtual | Self::Task)
    }

    /// Returns `true` if changes may be copied from a stream of this kind to its parent.
    pub fn copies_up(self) -> bool {
        matches!(self, Self::Development | Self::Task)
    }
}

/// Records that a changelist integrates another stream's changes through a
/// given changelist of that stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integration {
    /// The stream whose changes were integrated.
    pub source: DepotPath,
    /// The last changelist of `source` that is now integrated.
    pub through: ClNumber,
}

/// Everything about a submit except its changelist number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// The stream to submit to.
    pub stream: DepotPath,
    /// The submitting user.
    pub author: String,
    /// The submitting workspace, which must hold locks on exclusive files.
    pub workspace: String,
    /// The changelist description.
    pub description: String,
    /// The file changes.
    pub changes: Vec<FileChange>,
    /// The merge or copy this submit completes, if any.
    pub integration: Option<Integration>,
    /// The issue keys the changelist is linked to, in canonical form: those
    /// its description mentions and those it fixes.
    pub issues: Vec<String>,
    /// The linked issues the changelist fixes, for webhooks and queries.
    pub fixes: Vec<String>,
    /// The review whose latest version this submit is, if any. Changes that
    /// the review rules cover need one. A shelf without an open review is
    /// ignored.
    pub review: Option<ClNumber>,
    /// When the submit happened.
    pub at: Timestamp,
}

impl Submission {
    /// Returns whether the server made this submit itself, as for a
    /// copy-up, rather than a workspace. Such a submit is not bound by the
    /// exclusive locks a workspace must hold; it is marked by an empty
    /// workspace name, which only the server can produce.
    pub fn server_side(&self) -> bool {
        self.workspace.is_empty()
    }
}

/// A pending changelist stored on the server for others to see.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfContents {
    /// The stream the changes were made against.
    pub stream: DepotPath,
    /// The user who shelved the changes.
    pub owner: String,
    /// The workspace the changes were shelved from.
    pub workspace: String,
    /// The changelist description.
    pub description: String,
    /// The shelved changes.
    pub files: Vec<FileChange>,
    /// When the changes were shelved.
    pub at: Timestamp,
}

/// A published build artifact: a tree of files tied to a changelist of a
/// stream, outside the stream's history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactContents {
    /// The stream the artifact was built from.
    pub stream: DepotPath,
    /// The changelist the artifact was built from.
    pub cl: ClNumber,
    /// The artifact channel, such as `editor-win64`.
    pub name: String,
    /// Free-form labels such as `platform=win64`, sorted by key.
    pub tags: Vec<(String, String)>,
    /// The files, by artifact-relative path.
    pub files: Vec<(String, FileData)>,
    /// The publishing user.
    pub by: String,
    /// When the artifact was published.
    pub at: Timestamp,
}

/// A state change recorded in the metadata log.
///
/// Commands are applied by [`MetaDb::commit`](crate::MetaDb::commit). Commands
/// that need a fresh changelist number are proposed through dedicated
/// methods such as [`MetaDb::submit`](crate::MetaDb::submit).
///
/// The log keeps commands as they were encoded, so new variants are only
/// ever added at the end; changing an existing one needs a migration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    /// Creates a user. A user without a password hash is a service account
    /// that can only authenticate with tokens issued by an administrator.
    CreateUser {
        name: String,
        password_hash: Option<String>,
        admin: bool,
        /// Whether this is an edge server's account; see [`User::replica`].
        ///
        /// [`User::replica`]: crate::record::User::replica
        replica: bool,
        at: Timestamp,
    },
    /// Requires (`Some`) or stops requiring a TOTP code, with this base32
    /// secret, when `name` logs in with a password.
    SetTotp {
        name: String,
        secret: Option<String>,
    },
    /// Replaces the password hash of an existing user.
    SetPassword { name: String, password_hash: String },
    /// Records an access token, identified by the hash of its secret.
    IssueToken {
        token_hash: Hash,
        user: String,
        expires_at: Timestamp,
    },
    /// Removes an access token.
    RevokeToken { token_hash: Hash },
    /// Sets the members of a group, deleting the group if `members` is empty.
    SetGroup { name: String, members: Vec<String> },
    /// Replaces the protections table.
    SetProtections { source: String },
    /// Creates a depot.
    CreateDepot { name: String, at: Timestamp },
    /// Replaces the typemap of a depot.
    SetTypemap {
        depot: String,
        source: String,
        by: String,
        at: Timestamp,
    },
    /// Creates a stream at `//depot/stream`, branching from the head of
    /// `parent` if it has one.
    CreateStream {
        path: DepotPath,
        kind: StreamKind,
        parent: Option<DepotPath>,
        at: Timestamp,
    },
    /// Submits changelist `cl`.
    Submit {
        cl: ClNumber,
        submission: Submission,
    },
    /// Stores shelf `cl`, replacing it if it exists.
    Shelve {
        cl: ClNumber,
        contents: ShelfContents,
    },
    /// Deletes shelf `cl`.
    DeleteShelf { cl: ClNumber },
    /// Acquires exclusive locks on the stream-relative `paths`, all or none.
    AcquireLocks {
        holder: LockHolder,
        domain: LockDomain,
        paths: Vec<String>,
    },
    /// Releases locks on the stream-relative `paths`. With a `holder`, only
    /// that holder's locks are released; without one, any lock is.
    ReleaseLocks {
        holder: Option<LockHolder>,
        domain: LockDomain,
        paths: Vec<String>,
    },
    /// Publishes artifact `id`, then expires older artifacts of the same
    /// stream and name beyond the retention limit unless they are pinned.
    PublishArtifact { id: u64, contents: ArtifactContents },
    /// Pins or unpins an artifact; pinned artifacts never expire.
    PinArtifact { id: u64, pinned: bool },
    /// Deletes an artifact.
    DeleteArtifact { id: u64 },
    /// Keeps only the newest `keep_last` artifacts of `name` built from
    /// `stream`; `None` keeps all.
    SetArtifactRetention {
        stream: DepotPath,
        name: String,
        keep_last: Option<u32>,
    },
    /// Governs how changes are submitted and announced; see
    /// [`Governance`].
    Governance(Governance),
    /// Permanently removes files and their history: every revision of the
    /// matching paths, and the paths themselves from every changelist's tree
    /// and from shelves. A path ending in `/...` matches a directory.
    Obliterate { paths: Vec<String> },
    /// Replaces the imports of `stream`; see [`Import`].
    ///
    /// [`Import`]: crate::record::Import
    SetImports {
        stream: DepotPath,
        imports: Vec<crate::record::Import>,
    },
    /// Lets the first single sign-on as `sso_name`, the provider's user name
    /// as Kelp spells it, log in as `user` and link the two; `None`
    /// withdraws it.
    ExpectSso {
        user: String,
        sso_name: Option<String>,
    },
    /// Links `user` to the identity `subject` of the provider `issuer`.
    /// The user then logs in only through single sign-on: the password,
    /// the TOTP secret and every token are removed.
    LinkSso {
        user: String,
        issuer: String,
        subject: String,
    },
    /// Removes the single sign-on link of `user` and any expected one.
    UnlinkSso { user: String },
    /// Merges the user `from` into `into`, or renames `from` to `into` if
    /// no such user exists. Groups, tokens, locks, shelves, reviews and
    /// rules tables move to `into`; history keeps the name it was recorded
    /// under, and `from` remains reserved as an alias of `into`.
    MergeUser { from: String, into: String },
    /// Adds `user` to or removes them from the existing group `group`.
    /// Unlike [`Command::SetGroup`], this never deletes the group, and
    /// concurrent changes for different users do not overwrite each other.
    SetMembership {
        group: String,
        user: String,
        member: bool,
    },
    /// Disables or enables `user`. A disabled user cannot log in, and
    /// disabling revokes their tokens; their history and locks stay.
    SetDisabled { user: String, disabled: bool },
    /// Sets the label `name` to `stream` as of changelist `cl`, or deletes
    /// it if `target` is `None`.
    SetLabel {
        name: String,
        target: Option<(DepotPath, ClNumber)>,
        description: String,
        by: String,
        at: Timestamp,
    },
    /// Deletes a stream that has no child streams, imports, shelves or
    /// open reviews. Its changelists stay in the history, and its path
    /// cannot be used for a new stream.
    DeleteStream { path: DepotPath },
    /// Records that changelist `cl` moved each `(from, to)`: it deleted
    /// `from` and added `to`. The history of `to` then continues with that
    /// of `from`.
    RecordMoves {
        cl: ClNumber,
        moves: Vec<(DepotPath, DepotPath)>,
    },
}

impl Command {
    /// Returns a description of the command for the audit log, leaving out
    /// password hashes and token hashes.
    pub fn summary(&self) -> String {
        match self {
            Self::CreateUser {
                name,
                admin,
                replica,
                password_hash,
                ..
            } => {
                let kind = match (password_hash.is_some(), replica) {
                    (true, _) => "user",
                    (false, false) => "service account",
                    (false, true) => "edge account",
                };
                format!(
                    "create {kind} {name}{}",
                    if *admin { " (administrator)" } else { "" }
                )
            }
            Self::SetPassword { name, .. } => format!("set password of {name}"),
            Self::SetTotp { name, secret } => match secret {
                Some(_) => format!("enable two-factor login for {name}"),
                None => format!("disable two-factor login for {name}"),
            },
            Self::IssueToken {
                user, expires_at, ..
            } => format!("issue token for {user} until {expires_at}"),
            Self::RevokeToken { .. } => "revoke token".into(),
            Self::SetGroup { name, members } => {
                format!("set group {name} to [{}]", members.join(", "))
            }
            Self::SetProtections { .. } => "replace protections table".into(),
            Self::CreateDepot { name, .. } => format!("create depot //{name}"),
            Self::SetTypemap { depot, .. } => format!("replace typemap of //{depot}"),
            Self::CreateStream {
                path, kind, parent, ..
            } => match parent {
                Some(p) => format!("create {kind:?} stream {path} from {p}"),
                None => format!("create {kind:?} stream {path}"),
            },
            Self::Submit { cl, submission } => {
                format!(
                    "submit change {cl} to {} ({} files)",
                    submission.stream,
                    submission.changes.len()
                )
            }
            Self::Shelve { cl, contents } => format!(
                "shelve {cl} on {} ({} files)",
                contents.stream,
                contents.files.len()
            ),
            Self::DeleteShelf { cl } => format!("delete shelf {cl}"),
            Self::AcquireLocks { holder, paths, .. } => {
                format!(
                    "lock {} files for {}@{}",
                    paths.len(),
                    holder.user,
                    holder.workspace
                )
            }
            Self::ReleaseLocks { holder, paths, .. } => match holder {
                Some(h) => format!("unlock {} files of {}@{}", paths.len(), h.user, h.workspace),
                None => format!("force-unlock {} files", paths.len()),
            },
            Self::Obliterate { paths } => format!("obliterate {}", paths.join(", ")),
            Self::SetImports { stream, imports } => {
                let list: Vec<String> = imports.iter().map(ToString::to_string).collect();
                format!("set imports of {stream}: [{}]", list.join("; "))
            }
            Self::ExpectSso { user, sso_name } => match sso_name {
                Some(n) => format!("expect single sign-on as {n} for {user}"),
                None => format!("stop expecting single sign-on for {user}"),
            },
            Self::LinkSso {
                user,
                issuer,
                subject,
            } => format!("link {user} to {subject} at {issuer}"),
            Self::UnlinkSso { user } => format!("unlink single sign-on of {user}"),
            Self::MergeUser { from, into } => format!("merge user {from} into {into}"),
            Self::DeleteStream { path } => format!("delete stream {path}"),
            Self::RecordMoves { cl, moves } => format!("record {} moves in {cl}", moves.len()),
            Self::SetLabel { name, target, .. } => match target {
                Some((stream, cl)) => format!("label {stream}@{cl} {name}"),
                None => format!("delete label {name}"),
            },
            Self::SetDisabled { user, disabled } => match disabled {
                true => format!("disable user {user}"),
                false => format!("enable user {user}"),
            },
            Self::SetMembership {
                group,
                user,
                member,
            } => match member {
                true => format!("add {user} to group {group}"),
                false => format!("remove {user} from group {group}"),
            },
            Self::PublishArtifact { id, contents } => format!(
                "publish artifact {id} {} of {}@{} ({} files)",
                contents.name,
                contents.stream,
                contents.cl,
                contents.files.len()
            ),
            Self::PinArtifact { id, pinned } => {
                format!("{} artifact {id}", if *pinned { "pin" } else { "unpin" })
            }
            Self::DeleteArtifact { id } => format!("delete artifact {id}"),
            Self::SetArtifactRetention {
                stream,
                name,
                keep_last,
            } => match keep_last {
                Some(n) => format!("keep the last {n} artifacts {name} of {stream}"),
                None => format!("keep all artifacts {name} of {stream}"),
            },
            Self::Governance(g) => g.summary(),
        }
    }
}

/// Returns the content of `action`'s base, if it has one.
pub fn base_of(action: &FileAction) -> Option<ContentId> {
    match action {
        FileAction::Add { .. } => None,
        FileAction::Edit { base, .. } | FileAction::Delete { base } => Some(*base),
    }
}

impl From<Governance> for Command {
    fn from(g: Governance) -> Self {
        Self::Governance(g)
    }
}
