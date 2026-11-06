//! Requests, replies and the data types they carry.
//!
//! Paths inside a stream are stream-relative with `/` separators, such as
//! `Content/Hero.png`. Streams and files outside a stream are named by depot
//! path, such as `//game/main`.

use kelp_core::{ChunkId, ContentId, FileManifest, TreeId};
use serde::{Deserialize, Serialize};

/// A client request.
///
/// New variants are only ever appended, so older peers decode the variants
/// they know. Never insert a variant before another or change the fields of
/// an existing one, here or in any type a request or reply carries: postcard
/// identifies variants by position and fields by order, so such a change
/// makes peers misread each other. If a change of shape is unavoidable, bump
/// [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) and
/// [`MIN_PROTOCOL_VERSION`](crate::MIN_PROTOCOL_VERSION).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Exchanges a user name and password, and a TOTP code if the user has
    /// two-factor login, for an access token.
    Login {
        user: String,
        password: String,
        code: Option<String>,
    },
    /// Starts single sign-on with the server's OpenID Connect provider. The
    /// user approves the login in a browser while the client polls with
    /// [`Request::OidcPoll`].
    OidcStart,
    /// Asks whether the login of `device_code` was approved; replies with a
    /// token once it is, or [`Reply::AuthPending`] until then.
    OidcPoll {
        device_code: String,
    },
    /// Generates a TOTP secret for the user to add to an authenticator app.
    /// Nothing changes until [`Request::TotpConfirm`].
    TotpEnroll,
    /// Requires a TOTP code at login from now on, after checking that `code`
    /// matches `secret`.
    TotpConfirm {
        secret: String,
        code: String,
    },
    /// Stops requiring a TOTP code. Users turning it off for themselves give
    /// a current code; administrators can turn it off for anyone.
    TotpDisable {
        user: Option<String>,
        code: Option<String>,
    },
    /// Creates a user. Without a password the user is a service account that
    /// can only use tokens from [`Request::IssueToken`]. Requires an administrator.
    CreateUser {
        name: String,
        password: Option<String>,
        admin: bool,
        /// Creates an edge server's account, which may read the command log.
        /// It must be a service account and not an administrator.
        replica: bool,
    },
    /// Creates a depot and its `main` mainline stream. Requires an administrator.
    CreateDepot {
        name: String,
    },
    /// Lists the streams the user may read.
    ListStreams,
    /// Resolves a stream to the changelist and tree at `at`, or at its head.
    ResolveStream {
        stream: String,
        at: Option<u64>,
    },
    /// Computes file-level differences between two trees of `stream`, and
    /// looks up `paths` in `to`. Files the user may not read are omitted.
    DiffTree {
        stream: String,
        from: Option<TreeId>,
        to: TreeId,
        paths: Vec<String>,
    },
    /// Asks which chunks the server already stores.
    HasChunks {
        ids: Vec<ChunkId>,
    },
    /// Uploads `count` chunks, sent as [`MsgType::Chunk`](crate::MsgType::Chunk)
    /// frames after the request.
    PutChunks {
        count: u32,
    },
    /// Downloads chunks, answered with chunk frames before the reply.
    GetChunks {
        ids: Vec<ChunkId>,
    },
    /// Uploads manifests for content whose chunks are already stored.
    PutManifests {
        manifests: Vec<FileManifest>,
    },
    /// Downloads manifests.
    GetManifests {
        ids: Vec<ContentId>,
    },
    /// Submits a changelist to a stream from `workspace`, which must hold the
    /// locks of exclusive files.
    Submit {
        stream: String,
        workspace: String,
        description: String,
        changes: Vec<SubmitChange>,
        /// Records a merge-down from `(source stream, through changelist)`.
        integration: Option<(String, u64)>,
        /// The shelf this changelist was shelved as. If it has an open review,
        /// the submit must match the review's latest version and completes it.
        review: Option<u64>,
        /// Issue keys the changelist fixes, besides those its description
        /// mentions, which are linked too.
        fixes: Vec<String>,
        /// Files moved, as `(from, to)` depot paths: the changes delete
        /// `from` and add `to`, and the history of `to` continues `from`'s.
        moves: Vec<(String, String)>,
    },
    /// Lists the revisions of a file, newest first, continuing with the
    /// files it was moved from.
    Filelog {
        path: String,
    },
    /// Lists recent changelists, newest first.
    Changes {
        stream: Option<String>,
        limit: u32,
    },
    /// Describes one changelist.
    Describe {
        cl: u64,
    },
    /// Issues an access token for `user`. Requires an administrator.
    IssueToken {
        user: String,
        ttl_days: u32,
    },
    /// Sets the members of a group; an empty list deletes it. Requires an administrator.
    SetGroup {
        name: String,
        members: Vec<String>,
    },
    /// Lists groups and their members.
    ListGroups,
    /// Returns the protections table.
    GetProtections,
    /// Replaces the protections table. Requires an administrator.
    SetProtections {
        source: String,
    },
    /// Returns the typemap of a depot.
    GetTypemap {
        depot: String,
    },
    /// Replaces the typemap of a depot. Requires an administrator.
    SetTypemap {
        depot: String,
        source: String,
    },
    /// Locks stream-relative `paths` for `workspace`, all or none.
    AcquireLocks {
        stream: String,
        workspace: String,
        paths: Vec<String>,
    },
    /// Releases locks held by `workspace`, or any locks with `force`
    /// (administrators only).
    ReleaseLocks {
        stream: String,
        workspace: String,
        paths: Vec<String>,
        force: bool,
    },
    /// Lists the locks in a stream's lock domain.
    ListLocks {
        stream: String,
    },
    /// Creates a stream branching from `parent`.
    CreateStream {
        path: String,
        kind: StreamKind,
        parent: Option<String>,
    },
    /// Returns what `target` has integrated from `source` and what `source`
    /// has now, so that a client can merge the difference.
    GetIntegration {
        target: String,
        source: String,
    },
    /// Copies a stream's changes into its parent as one changelist.
    CopyUp {
        stream: String,
        description: String,
    },
    /// Shelves changes, replacing `shelf` if given.
    Shelve {
        shelf: Option<u64>,
        stream: String,
        workspace: String,
        description: String,
        files: Vec<SubmitChange>,
    },
    /// Returns a shelf.
    GetShelf {
        cl: u64,
    },
    /// Deletes a shelf. Requires its owner or an administrator.
    DeleteShelf {
        cl: u64,
    },
    /// Deletes stored content, manifests and tree nodes nothing refers to.
    /// Requires an administrator.
    Gc {
        dry_run: bool,
    },
    /// Permanently removes files and their history; a path ending in `/...`
    /// names a directory. Requires an administrator.
    Obliterate {
        paths: Vec<String>,
    },
    /// Returns up to `limit` audit entries from log index `from`. Requires an
    /// administrator.
    AuditLog {
        from: u64,
        limit: u32,
    },
    /// Checks the audit chain against the command log. Requires an administrator.
    VerifyAudit,
    /// Writes a backup into the directory `dest` on the server's machine;
    /// see `kelp-server restore`. Requires an administrator.
    Backup {
        dest: String,
        metadata_only: bool,
    },
    /// Checks that every file in use can be read; with `deep`, reads and
    /// hashes every chunk. Requires an administrator.
    Fsck {
        deep: bool,
    },
    /// Moves chunks that no stream's current files use to cold storage.
    /// Requires an administrator and tiered storage.
    Tier,
    /// Publishes a build artifact of `stream` at changelist `cl`. Requires
    /// write access to the stream.
    PublishArtifact {
        stream: String,
        cl: u64,
        name: String,
        tags: Vec<(String, String)>,
        files: Vec<(String, FileInfo)>,
    },
    /// Finds artifacts `name` of `stream` at or below changelist `at` with
    /// `tags`, newest first.
    FindArtifacts {
        stream: String,
        name: String,
        at: Option<u64>,
        tags: Vec<(String, String)>,
        limit: u32,
    },
    /// Lists an artifact's files.
    ArtifactFiles {
        id: u64,
    },
    /// Describes one artifact; replies with a single-element list.
    GetArtifact {
        id: u64,
    },
    /// Pins or unpins an artifact. Requires an administrator.
    PinArtifact {
        id: u64,
        pinned: bool,
    },
    /// Deletes an artifact. Requires an administrator.
    DeleteArtifact {
        id: u64,
    },
    /// Keeps only the newest `keep_last` artifacts `name` of `stream`, or all
    /// if `None`. Requires an administrator.
    SetArtifactRetention {
        stream: String,
        name: String,
        keep_last: Option<u32>,
    },
    /// Returns up to `limit` command log entries from index `from`, waiting
    /// up to `wait_ms` for the first if there is none yet. Requires an edge
    /// account or an administrator.
    ReadLog {
        from: u64,
        limit: u32,
        wait_ms: u32,
    },
    /// Opens a review of one of the user's shelves.
    OpenReview {
        shelf: u64,
        reviewers: Vec<String>,
    },
    /// Approves (`Some(true)`) or rejects `version` of a review, or
    /// withdraws the user's vote. Fails with [`ErrorKind::Conflict`] if the
    /// review has a newer version, which the user has not seen.
    Vote {
        shelf: u64,
        version: u32,
        approve: Option<bool>,
    },
    /// Reports the outcome of an automated check of a review version.
    /// Requires write access to the review's files.
    ReportStatus {
        shelf: u64,
        version: u32,
        context: String,
        state: CheckState,
        url: Option<String>,
    },
    /// Closes a review without submitting. Requires its author or an
    /// administrator.
    CloseReview {
        shelf: u64,
    },
    GetReview {
        shelf: u64,
    },
    /// Lists reviews, newest first.
    ListReviews {
        stream: Option<String>,
        open_only: bool,
    },
    /// Returns the review rules table.
    GetReviewRules,
    /// Replaces the review rules table. Requires an administrator.
    SetReviewRules {
        source: String,
    },
    /// Creates or replaces a webhook. It receives events whose kind starts
    /// with one of `events` and that touch one of `paths`; an empty list
    /// matches everything. Requires an administrator.
    SetWebhook {
        name: String,
        url: String,
        secret: String,
        events: Vec<String>,
        paths: Vec<String>,
    },
    /// Requires an administrator.
    DeleteWebhook {
        name: String,
    },
    /// Lists webhooks without their secrets. Requires an administrator.
    ListWebhooks,
    /// Lists the newest deliveries, of one webhook or all. Requires an
    /// administrator.
    Deliveries {
        webhook: Option<String>,
        limit: u32,
    },
    /// Sends a delivery again. Requires an administrator.
    Redeliver {
        id: u64,
    },
    /// Registers an uploaded component as plugin `name` at `version`. The
    /// server checks that it is a valid plugin. Requires an administrator.
    RegisterPlugin {
        name: String,
        version: String,
        content: ContentId,
        size: u64,
    },
    /// Requires an administrator.
    DeletePlugin {
        name: String,
        version: String,
    },
    GetPlugin {
        name: String,
        version: String,
    },
    ListPlugins,
    /// Describes an issue: the changelists linked to it and the streams it
    /// has reached.
    GetIssue {
        key: String,
    },
    /// Corrects the issues a changelist is linked to. Requires its author or
    /// an administrator.
    LinkIssues {
        cl: u64,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Lists the issues linked to changes that `stream` contains at `at`
    /// (default: its head) but did not contain at `since`.
    IssuesIn {
        stream: String,
        at: Option<u64>,
        since: Option<u64>,
    },
    /// Returns the imports of a stream.
    GetImports {
        stream: String,
    },
    /// Replaces the imports of a stream; an empty list removes them. Requires
    /// an administrator.
    SetImports {
        stream: String,
        imports: Vec<ImportInfo>,
    },
    /// Describes the commit group as this member sees it. Requires an
    /// administrator.
    ClusterStatus,
    /// Like [`Request::OidcPoll`], but links the approved single sign-on
    /// identity to the logged-in user instead of logging in. The user's
    /// password and tokens stop working; the reply carries a new token.
    OidcLink {
        device_code: String,
    },
    /// Lets the first single sign-on as `sso_name` log in as `user` and
    /// link the two; `None` withdraws it. Requires an administrator.
    ExpectSso {
        user: String,
        sso_name: Option<String>,
    },
    /// Removes the single sign-on link of `user`. Requires an
    /// administrator.
    UnlinkSso {
        user: String,
    },
    /// Merges the user `from` into `into`, or renames `from` if `into` does
    /// not exist. Requires an administrator.
    MergeUser {
        from: String,
        into: String,
    },
    /// Describes a user, found by current or former name. Requires an
    /// administrator.
    GetUser {
        name: String,
    },
    /// Lists every user. Requires an administrator.
    ListUsers,
    /// Revokes the token this connection logged in with.
    Logout,
    /// Sets the label `name` to `stream` as of changelist `cl`, or its
    /// head. Moving a label needs its owner or an administrator.
    SetLabel {
        name: String,
        stream: String,
        cl: Option<u64>,
        description: String,
    },
    /// Deletes a label. Requires its owner or an administrator.
    DeleteLabel {
        name: String,
    },
    GetLabel {
        name: String,
    },
    /// Lists the labels of the streams the user can see.
    ListLabels,
    /// Deletes a stream, keeping its history. Requires an administrator,
    /// except for task streams, which anyone who can see them may delete.
    DeleteStream {
        stream: String,
    },
    /// Changes the password of `user`, or of the logged-in user if `None`.
    /// Changing one's own password needs the `current` one; an
    /// administrator may set anyone else's without it.
    SetPassword {
        user: Option<String>,
        current: Option<String>,
        password: String,
    },
    /// Disables or enables a user. A disabled user cannot log in and loses
    /// their tokens. Requires an administrator.
    SetDisabled {
        user: String,
        disabled: bool,
    },
    /// Changes the display name or addresses of `user`, or of the
    /// logged-in user if `None`. Anyone may change their own; changing
    /// another's requires an administrator.
    EditProfile {
        user: Option<String>,
        change: ProfileChange,
    },
    /// A message between members of a commit group, authenticated by the
    /// group's shared secret rather than a user.
    Raft {
        secret: String,
        /// The sender's database format version; members only talk to
        /// members with the same.
        format: u64,
        rpc: RaftRpc,
        /// The encoded request of `rpc`.
        payload: Vec<u8>,
    },
    /// Lists the shelves with files the user can read, newest first: those of
    /// `stream` and of `owner` if given, at most `limit` of them, or all if
    /// `limit` is 0.
    ListShelves {
        stream: Option<String>,
        owner: Option<String>,
        limit: u32,
    },
    /// Downloads chunks `ids`, each a chunk of the manifest of one of
    /// `contents`, sent as [`MsgType::Chunk`](crate::MsgType::Chunk) frames
    /// before the reply.
    ///
    /// Unlike [`GetChunks`](Self::GetChunks), naming the content lets the
    /// server check that the user may read a path the content appears at.
    /// Servers that offer it advertise
    /// [`Capabilities::CONTENT_ACCESS`](crate::Capabilities::CONTENT_ACCESS).
    GetContentChunks {
        contents: Vec<ContentId>,
        ids: Vec<ChunkId>,
    },
    /// Lists what is below the stream-relative directory `path` of `stream`,
    /// depth first. An empty `path` names the stream's root, and a file lists
    /// only itself.
    ///
    /// Entries the user may not read are omitted, as are directories with
    /// nothing readable.
    ListDir {
        stream: String,
        path: String,
        options: ListOptions,
    },
}

/// What a request does, which decides where it is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestClass {
    /// Logs in; an edge sends it to the commit server.
    Session,
    /// Reads replicated state, which any server answers itself.
    Read,
    /// Reads state that only the commit server has.
    Authoritative,
    /// Uploads content or asks what needs uploading, which the commit
    /// server must see so that garbage collection keeps it.
    Upload,
    /// Changes state; only the commit server or group leader applies it.
    Write,
    /// A message between members of a commit group.
    Member,
}

impl Request {
    /// Returns what the request does.
    pub fn class(&self) -> RequestClass {
        match self {
            Self::Login { .. } | Self::OidcStart | Self::OidcPoll { .. } => RequestClass::Session,
            Self::Raft { .. } => RequestClass::Member,
            Self::HasChunks { .. } | Self::PutChunks { .. } | Self::PutManifests { .. } => {
                RequestClass::Upload
            }
            Self::CreateUser { .. }
            | Self::IssueToken { .. }
            | Self::SetGroup { .. }
            | Self::SetProtections { .. }
            | Self::CreateDepot { .. }
            | Self::SetTypemap { .. }
            | Self::SetImports { .. }
            | Self::CreateStream { .. }
            | Self::CopyUp { .. }
            | Self::Submit { .. }
            | Self::AcquireLocks { .. }
            | Self::ReleaseLocks { .. }
            | Self::Shelve { .. }
            | Self::DeleteShelf { .. }
            | Self::Gc { .. }
            | Self::Obliterate { .. }
            | Self::Tier
            | Self::Backup { .. }
            | Self::PublishArtifact { .. }
            | Self::PinArtifact { .. }
            | Self::DeleteArtifact { .. }
            | Self::SetArtifactRetention { .. }
            | Self::OpenReview { .. }
            | Self::Vote { .. }
            | Self::ReportStatus { .. }
            | Self::CloseReview { .. }
            | Self::SetReviewRules { .. }
            | Self::SetWebhook { .. }
            | Self::DeleteWebhook { .. }
            | Self::Redeliver { .. }
            | Self::RegisterPlugin { .. }
            | Self::DeletePlugin { .. }
            | Self::TotpConfirm { .. }
            | Self::LinkIssues { .. }
            | Self::TotpDisable { .. }
            | Self::OidcLink { .. }
            | Self::ExpectSso { .. }
            | Self::UnlinkSso { .. }
            | Self::MergeUser { .. }
            | Self::SetPassword { .. }
            | Self::SetDisabled { .. }
            | Self::EditProfile { .. }
            | Self::Logout
            | Self::SetLabel { .. }
            | Self::DeleteLabel { .. }
            | Self::DeleteStream { .. } => RequestClass::Write,
            Self::Deliveries { .. } => RequestClass::Authoritative,
            Self::TotpEnroll
            | Self::ListStreams
            | Self::ResolveStream { .. }
            | Self::DiffTree { .. }
            | Self::ListDir { .. }
            | Self::GetChunks { .. }
            | Self::GetManifests { .. }
            | Self::GetContentChunks { .. }
            | Self::ListShelves { .. }
            | Self::Filelog { .. }
            | Self::Changes { .. }
            | Self::Describe { .. }
            | Self::ListGroups
            | Self::GetProtections
            | Self::GetTypemap { .. }
            | Self::ListLocks { .. }
            | Self::GetIntegration { .. }
            | Self::GetShelf { .. }
            | Self::AuditLog { .. }
            | Self::VerifyAudit
            | Self::Fsck { .. }
            | Self::FindArtifacts { .. }
            | Self::ArtifactFiles { .. }
            | Self::GetArtifact { .. }
            | Self::ReadLog { .. }
            | Self::GetReview { .. }
            | Self::ListReviews { .. }
            | Self::GetReviewRules
            | Self::ListWebhooks
            | Self::GetPlugin { .. }
            | Self::ListPlugins
            | Self::GetIssue { .. }
            | Self::IssuesIn { .. }
            | Self::GetImports { .. }
            | Self::ClusterStatus
            | Self::GetUser { .. }
            | Self::ListUsers
            | Self::GetLabel { .. }
            | Self::ListLabels => RequestClass::Read,
        }
    }

    /// Returns the name of the request, for logs; it never includes the
    /// request's contents.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Login { .. } => "Login",
            Self::OidcStart => "OidcStart",
            Self::OidcPoll { .. } => "OidcPoll",
            Self::TotpEnroll => "TotpEnroll",
            Self::TotpConfirm { .. } => "TotpConfirm",
            Self::TotpDisable { .. } => "TotpDisable",
            Self::CreateUser { .. } => "CreateUser",
            Self::CreateDepot { .. } => "CreateDepot",
            Self::ListStreams => "ListStreams",
            Self::ResolveStream { .. } => "ResolveStream",
            Self::DiffTree { .. } => "DiffTree",
            Self::HasChunks { .. } => "HasChunks",
            Self::PutChunks { .. } => "PutChunks",
            Self::GetChunks { .. } => "GetChunks",
            Self::PutManifests { .. } => "PutManifests",
            Self::GetManifests { .. } => "GetManifests",
            Self::Submit { .. } => "Submit",
            Self::Filelog { .. } => "Filelog",
            Self::Changes { .. } => "Changes",
            Self::Describe { .. } => "Describe",
            Self::IssueToken { .. } => "IssueToken",
            Self::SetGroup { .. } => "SetGroup",
            Self::ListGroups => "ListGroups",
            Self::GetProtections => "GetProtections",
            Self::SetProtections { .. } => "SetProtections",
            Self::GetTypemap { .. } => "GetTypemap",
            Self::SetTypemap { .. } => "SetTypemap",
            Self::AcquireLocks { .. } => "AcquireLocks",
            Self::ReleaseLocks { .. } => "ReleaseLocks",
            Self::ListLocks { .. } => "ListLocks",
            Self::CreateStream { .. } => "CreateStream",
            Self::GetIntegration { .. } => "GetIntegration",
            Self::CopyUp { .. } => "CopyUp",
            Self::Shelve { .. } => "Shelve",
            Self::GetShelf { .. } => "GetShelf",
            Self::DeleteShelf { .. } => "DeleteShelf",
            Self::Gc { .. } => "Gc",
            Self::Obliterate { .. } => "Obliterate",
            Self::AuditLog { .. } => "AuditLog",
            Self::VerifyAudit => "VerifyAudit",
            Self::Backup { .. } => "Backup",
            Self::Fsck { .. } => "Fsck",
            Self::Tier => "Tier",
            Self::PublishArtifact { .. } => "PublishArtifact",
            Self::FindArtifacts { .. } => "FindArtifacts",
            Self::ArtifactFiles { .. } => "ArtifactFiles",
            Self::GetArtifact { .. } => "GetArtifact",
            Self::PinArtifact { .. } => "PinArtifact",
            Self::DeleteArtifact { .. } => "DeleteArtifact",
            Self::SetArtifactRetention { .. } => "SetArtifactRetention",
            Self::ReadLog { .. } => "ReadLog",
            Self::OpenReview { .. } => "OpenReview",
            Self::Vote { .. } => "Vote",
            Self::ReportStatus { .. } => "ReportStatus",
            Self::CloseReview { .. } => "CloseReview",
            Self::GetReview { .. } => "GetReview",
            Self::ListReviews { .. } => "ListReviews",
            Self::GetReviewRules => "GetReviewRules",
            Self::SetReviewRules { .. } => "SetReviewRules",
            Self::SetWebhook { .. } => "SetWebhook",
            Self::DeleteWebhook { .. } => "DeleteWebhook",
            Self::ListWebhooks => "ListWebhooks",
            Self::Deliveries { .. } => "Deliveries",
            Self::Redeliver { .. } => "Redeliver",
            Self::RegisterPlugin { .. } => "RegisterPlugin",
            Self::DeletePlugin { .. } => "DeletePlugin",
            Self::GetPlugin { .. } => "GetPlugin",
            Self::ListPlugins => "ListPlugins",
            Self::GetIssue { .. } => "GetIssue",
            Self::LinkIssues { .. } => "LinkIssues",
            Self::IssuesIn { .. } => "IssuesIn",
            Self::GetImports { .. } => "GetImports",
            Self::SetImports { .. } => "SetImports",
            Self::ClusterStatus => "ClusterStatus",
            Self::OidcLink { .. } => "OidcLink",
            Self::ExpectSso { .. } => "ExpectSso",
            Self::UnlinkSso { .. } => "UnlinkSso",
            Self::MergeUser { .. } => "MergeUser",
            Self::GetUser { .. } => "GetUser",
            Self::ListUsers => "ListUsers",
            Self::Logout => "Logout",
            Self::SetLabel { .. } => "SetLabel",
            Self::DeleteLabel { .. } => "DeleteLabel",
            Self::GetLabel { .. } => "GetLabel",
            Self::ListLabels => "ListLabels",
            Self::DeleteStream { .. } => "DeleteStream",
            Self::SetPassword { .. } => "SetPassword",
            Self::SetDisabled { .. } => "SetDisabled",
            Self::EditProfile { .. } => "EditProfile",
            Self::Raft { .. } => "Raft",
            Self::ListShelves { .. } => "ListShelves",
            Self::GetContentChunks { .. } => "GetContentChunks",
            Self::ListDir { .. } => "ListDir",
        }
    }
}

/// One change to a user's display name or addresses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileChange {
    /// Sets the display name, or clears it with `None`.
    DisplayName(Option<String>),
    /// Adds an address; the first one added is the primary one. An address
    /// belongs to one user only.
    AddEmail(String),
    RemoveEmail(String),
    /// Makes an address the user has the primary one.
    PrimaryEmail(String),
}

/// The kinds of message members of a commit group exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaftRpc {
    AppendEntries,
    Vote,
    InstallSnapshot,
    /// Asks the leader how far a member must have applied the log to
    /// reflect everything committed so far.
    ReadIndex,
    /// Asks a member whether it belongs to a group yet.
    Probe,
    /// Asks the leader to commit a change for a member that turned out not
    /// to lead.
    Propose,
}

/// A successful reply.
///
/// Like [`Request`], only ever extended by appending variants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Token {
        token: String,
        user: String,
        expires_at: u64,
    },
    Streams(Vec<StreamInfo>),
    Stream(StreamInfo),
    TreeDiff {
        changes: Vec<FileDelta>,
        lookups: Vec<Option<FileInfo>>,
    },
    Has(Vec<bool>),
    Manifests(Vec<Option<FileManifest>>),
    Submitted {
        cl: u64,
        root: TreeId,
    },
    Filelog(Vec<RevisionInfo>),
    Changes(Vec<ChangeSummary>),
    Describe(ChangeDetail),
    Groups(Vec<GroupInfo>),
    /// A text document such as a typemap or protections table, if set.
    Text(Option<String>),
    Locks(Vec<LockInfo>),
    Integration(IntegrationInfo),
    Shelved {
        cl: u64,
    },
    Shelf(ShelfInfo),
    Gc(GcReport),
    Audit(Vec<AuditInfo>),
    AuditVerified {
        entries: u64,
        head: String,
    },
    Tiered {
        chunks: u64,
        bytes: u64,
    },
    Published {
        id: u64,
    },
    Artifacts(Vec<ArtifactInfo>),
    ArtifactFiles(Vec<(String, FileInfo)>),
    Log {
        entries: Vec<LogEntry>,
        /// The index of the server's last log entry.
        head: u64,
        /// The server's database format version; replicas must have the same.
        format: u64,
        /// A digest of every stream's head and tree as of `head`, which a
        /// replica that has applied exactly that far compares with its own.
        digest: kelp_core::Hash,
    },
    Review(ReviewInfo),
    Reviews(Vec<ReviewSummary>),
    BackedUp {
        log_index: u64,
        chunks_copied: u64,
        chunks_present: u64,
        bytes_copied: u64,
    },
    Fsck(FsckInfo),
    Webhooks(Vec<WebhookInfo>),
    Deliveries(Vec<DeliveryInfo>),
    Plugin(PluginInfo),
    Plugins(Vec<PluginInfo>),
    DeviceCode(DeviceCode),
    /// The single sign-on login has not been approved yet.
    AuthPending,
    TotpSecret {
        /// The secret in base32.
        secret: String,
        /// An `otpauth://` URI for authenticator apps.
        uri: String,
    },
    Issue(IssueDetail),
    /// Issue keys and the changelists linked to them.
    IssueKeys(Vec<(String, Vec<u64>)>),
    Imports(Vec<ImportInfo>),
    /// The encoded response to a [`Request::Raft`].
    Raft(Vec<u8>),
    Cluster(ClusterInfo),
    User(UserInfo),
    Users(Vec<UserInfo>),
    Label(LabelInfo),
    Labels(Vec<LabelInfo>),
    Shelves(Vec<ShelfSummary>),
    DirEntries(Vec<DirEntry>),
}

/// A name for a stream as of a changelist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelInfo {
    pub name: String,
    pub stream: String,
    pub cl: u64,
    pub description: String,
    pub owner: String,
    pub at: u64,
}

/// A user as an administrator sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub admin: bool,
    /// Whether the user logs in with a password.
    pub password: bool,
    /// Whether this is an edge server's account.
    pub replica: bool,
    /// The linked single sign-on identity, as `subject@issuer`.
    pub sso: Option<String>,
    /// The single sign-on name a first sign-on is expected under.
    pub expected_sso: Option<String>,
    /// Former names, which still refer to this user.
    pub aliases: Vec<String>,
    /// Whether the user is disabled.
    pub disabled: bool,
    /// The name shown instead of `name`, if the user set one.
    pub display_name: Option<String>,
    /// The addresses of the user, the primary one first.
    pub emails: Vec<String>,
}

/// A commit group as one member sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterInfo {
    /// The identifier of the member that answered.
    pub member: u64,
    pub leader: Option<u64>,
    /// The members that vote, which may differ from the configured members
    /// while the group grows.
    pub voters: Vec<u64>,
    /// The index of the last log entry the member has applied.
    pub applied: Option<u64>,
}

/// What Kelp knows about an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDetail {
    pub key: String,
    /// The linked changelists the user may see, oldest first.
    pub changes: Vec<ChangeSummary>,
    /// The streams containing a linked changelist, with the changelist
    /// through which it arrived, or `0` if the stream was created with it.
    pub reached: Vec<(String, u64)>,
}

/// Where and how to approve a single sign-on login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCode {
    /// What the client polls with.
    pub device_code: String,
    /// What the user enters at `verification_uri`.
    pub user_code: String,
    pub verification_uri: String,
    /// The URI with the code filled in, if the provider offers one.
    pub verification_uri_complete: Option<String>,
    /// Seconds to wait between polls.
    pub interval: u32,
    /// Seconds until the codes expire.
    pub expires_in: u32,
}

/// A registered plugin version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    /// The component file, fetched like any other content.
    pub content: ContentId,
    pub size: u64,
    /// The worlds it implements, such as `merger`.
    pub worlds: Vec<String>,
}

/// The findings of [`Request::Fsck`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsckInfo {
    pub audit_entries: u64,
    pub contents: u64,
    pub chunks: u64,
    pub errors: Vec<String>,
    pub missing_manifests: Vec<ContentId>,
    pub missing_chunks: Vec<ChunkId>,
    pub corrupt_chunks: Vec<ChunkId>,
}

/// A webhook, without its secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookInfo {
    pub name: String,
    pub url: String,
    pub events: Vec<String>,
    pub paths: Vec<String>,
}

/// One event sent, or to be sent, to a webhook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryInfo {
    pub id: u64,
    pub webhook: String,
    pub kind: String,
    pub attempts: u32,
    pub delivered: bool,
    /// Whether another attempt is scheduled.
    pub pending: bool,
    /// The HTTP status or error of the last attempt.
    pub last_result: Option<String>,
}

/// The state of an automated check of a review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
}

/// Where a review is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    Open,
    /// Submitted as this changelist.
    Submitted(u64),
    Closed,
}

/// A review and everything said about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInfo {
    pub summary: ReviewSummary,
    pub reviewers: Vec<String>,
    /// The files of the latest version that the user may read.
    pub files: Vec<SubmitChange>,
    /// `(user, version, approve)`.
    pub votes: Vec<(String, u32, bool)>,
    pub statuses: Vec<StatusInfo>,
    /// What the review rules require and whether each is met.
    pub checks: Vec<(String, bool)>,
}

/// A review as listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub shelf: u64,
    pub stream: String,
    pub author: String,
    /// The author's display name, if they set one.
    pub author_name: Option<String>,
    pub description: String,
    /// The latest version.
    pub version: u32,
    pub state: ReviewState,
    /// Whether the latest version meets every requirement.
    pub ready: bool,
}

/// A status report on a review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub context: String,
    pub version: u32,
    pub state: CheckState,
    pub url: Option<String>,
    pub by: String,
}

/// One command of the log; see [`Request::ReadLog`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub index: u64,
    pub actor: String,
    /// The encoded command, opaque to the protocol.
    pub command: Vec<u8>,
    /// The audit hash after this entry.
    pub hash: kelp_core::Hash,
}

/// The category of a failed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    Unauthenticated,
    Forbidden,
    NotFound,
    AlreadyExists,
    /// The request conflicts with changes made by others.
    Conflict,
    Invalid,
    Internal,
    /// An edge server cannot reach the commit server.
    Unavailable,
    /// The login needs a TOTP code.
    CodeRequired,
    /// The server stopped leading its commit group before it could log the
    /// change, which was not made; sending the request again is safe.
    NotLeader,
}

/// A failed reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct ErrorReply {
    pub kind: ErrorKind,
    pub message: String,
}

impl ErrorReply {
    /// Creates an error reply.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// The role of a stream in the branching model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Mainline,
    Development,
    Release,
    Virtual,
    Task,
}

/// A subtree of another stream shown inside a stream.
///
/// The files of `source` below `from` appear below `to` in the importing
/// stream, as of changelist `at`, or as the source changes when `at` is
/// `None`. Changes to a writable import are submitted to `source`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportInfo {
    pub source: String,
    pub from: String,
    pub to: String,
    pub at: Option<u64>,
    pub writable: bool,
}

/// A stream at a particular changelist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub path: String,
    pub kind: StreamKind,
    pub parent: Option<String>,
    /// The changelist the tree belongs to; `0` before the first submit.
    pub cl: u64,
    pub root: TreeId,
}

/// The content and attributes of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub content: ContentId,
    pub size: u64,
    pub executable: bool,
}

/// A file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDelta {
    pub path: String,
    pub old: Option<FileInfo>,
    pub new: Option<FileInfo>,
}

/// What [`Request::ListDir`] lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListOptions {
    /// The changelist to list as of; defaults to the head.
    pub at: Option<u64>,
    /// How many levels to list, with `1` being the directory's own entries;
    /// defaults to all of them.
    pub depth: Option<u32>,
    /// Lists only directories.
    pub dirs_only: bool,
    /// Fills in [`DirEntry::last`], which takes a lookup per file.
    pub details: bool,
}

/// An entry of a stream's file tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// The stream-relative path.
    pub path: String,
    pub kind: DirEntryKind,
    /// The revision that gave a file its content, if asked for and known.
    pub last: Option<LastChange>,
}

/// The revision that gave a file its content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastChange {
    pub rev: u32,
    pub cl: u64,
    pub author: String,
    /// The display name of the author, if set.
    pub author_name: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub submitted_at: u64,
}

/// What a [`DirEntry`] is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirEntryKind {
    File(FileInfo),
    Dir,
    Symlink { target: String },
}

/// One file of a submitted changelist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitChange {
    pub path: String,
    pub action: SubmitAction,
}

/// What a submitted changelist does to a file.
///
/// `base` is the content the change was made against. The server rejects the
/// submit if the file has changed since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmitAction {
    Add { file: FileInfo },
    Edit { base: ContentId, file: FileInfo },
    Delete { base: ContentId },
}

/// The kind of change a revision records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAction {
    Add,
    Edit,
    Delete,
}

/// One revision of a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionInfo {
    /// The path of the revision, which differs from the file asked about
    /// for revisions from before a move.
    pub path: String,
    pub rev: u32,
    pub cl: u64,
    pub action: FileAction,
    pub content: Option<ContentId>,
    pub size: u64,
    /// The content was discarded under the typemap's `retain` limit.
    pub purged: bool,
}

/// A submitted changelist without its files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub cl: u64,
    pub stream: String,
    pub author: String,
    /// The author's display name, if they set one.
    pub author_name: Option<String>,
    /// The author's primary address, if they have one.
    pub author_email: Option<String>,
    pub description: String,
    /// Milliseconds since the Unix epoch.
    pub submitted_at: u64,
    /// The issue keys the changelist is linked to.
    pub issues: Vec<String>,
    /// The linked issues it fixes.
    pub fixes: Vec<String>,
}

/// A submitted changelist with its files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeDetail {
    pub summary: ChangeSummary,
    /// Depot paths and what happened to them.
    pub files: Vec<(String, FileAction)>,
}

/// A group and its members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    pub members: Vec<String>,
}

/// A held exclusive lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    /// The stream-relative path.
    pub path: String,
    pub user: String,
    pub workspace: String,
}

/// The state of integration from one stream into another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInfo {
    /// The last changelist of the source already integrated, and the source
    /// tree as of then: the merge base.
    pub base_cl: u64,
    pub base_root: TreeId,
    /// The source's head changelist and tree.
    pub source_cl: u64,
    pub source_root: TreeId,
}

/// A shelved changelist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfInfo {
    pub cl: u64,
    pub stream: String,
    pub owner: String,
    pub workspace: String,
    pub description: String,
    pub files: Vec<SubmitChange>,
}

/// What a garbage collection found or removed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub dry_run: bool,
    pub chunks_kept: u64,
    pub chunks_deleted: u64,
    pub manifests_deleted: u64,
    pub nodes_deleted: u64,
    pub packs_compacted: u64,
    pub bytes_freed: u64,
}

/// One entry of the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditInfo {
    pub index: u64,
    pub actor: String,
    pub summary: String,
    /// The entry's chain hash as hex.
    pub hash: String,
}

/// A published build artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub id: u64,
    pub stream: String,
    pub cl: u64,
    pub name: String,
    pub tags: Vec<(String, String)>,
    pub files: u64,
    pub bytes: u64,
    pub by: String,
    /// Milliseconds since the Unix epoch.
    pub at: u64,
    pub pinned: bool,
}

/// A shelf as listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShelfSummary {
    pub cl: u64,
    pub stream: String,
    pub owner: String,
    /// The owner's display name, if they set one.
    pub owner_name: Option<String>,
    pub workspace: String,
    pub description: String,
    /// The number of shelved files the user may read.
    pub files: u32,
    /// When the files were shelved, in milliseconds since the Unix epoch.
    pub at: u64,
}
