//! Commands that govern how changes are submitted and announced, rather than
//! the changes themselves: the review gate, issue links, webhooks and
//! plugins.
//!
//! They are logged and replicated like every other [`Command`], so edges can
//! answer queries about them and the audit chain records who approved what,
//! but they are kept apart from the core version control state machine.
//!
//! [`Command`]: crate::Command

use kelp_core::{ClNumber, ContentId};
use serde::{Deserialize, Serialize};

use crate::Timestamp;

/// A command of the governance layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Governance {
    /// Creates or replaces the webhook `name`, which receives the events
    /// whose kind starts with one of `events` (all if empty) and that touch
    /// one of `paths` (all if empty).
    SetWebhook {
        name: String,
        url: String,
        /// The key deliveries are signed with.
        secret: String,
        events: Vec<String>,
        paths: Vec<String>,
    },
    /// Deletes the webhook `name`.
    DeleteWebhook { name: String },
    /// Links changelist `cl` to the issues `add` and unlinks it from
    /// `remove`, to correct links after the submit.
    LinkIssues {
        cl: ClNumber,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Registers version `version` of plugin `name`, whose component is the
    /// file `content`. Typemaps refer to it as `plugin:<name>@<version>`.
    /// A registered version cannot be replaced, only deleted.
    RegisterPlugin {
        name: String,
        version: String,
        content: ContentId,
        size: u64,
        /// The worlds the component implements, as checked by the server.
        worlds: Vec<String>,
        by: String,
        at: Timestamp,
    },
    /// Deletes a plugin version.
    DeletePlugin { name: String, version: String },
    /// Opens a review of shelf `shelf`, which `by` must own.
    OpenReview {
        shelf: ClNumber,
        reviewers: Vec<String>,
        by: String,
        at: Timestamp,
    },
    /// Records `user`'s vote on the latest version of a review, or clears
    /// it if `approve` is `None`.
    Vote {
        shelf: ClNumber,
        user: String,
        approve: Option<bool>,
        at: Timestamp,
    },
    /// Records the outcome of an automated check of a review version.
    ReportStatus {
        shelf: ClNumber,
        version: u32,
        context: String,
        state: crate::record::CheckState,
        url: Option<String>,
        by: String,
        at: Timestamp,
    },
    /// Closes an open review without submitting.
    CloseReview { shelf: ClNumber },
    /// Replaces the review rules table; see [`crate::review`].
    SetReviewRules { source: String },
    /// Records `user`'s vote on `version` of a review, or clears it if
    /// `approve` is `None`. Fails if the review has a newer version, so a
    /// vote always concerns what the reviewer saw.
    VoteOn {
        shelf: ClNumber,
        user: String,
        version: u32,
        approve: Option<bool>,
        at: Timestamp,
    },
}

impl Governance {
    /// Returns a description of the command for the audit log, leaving out
    /// secrets.
    pub fn summary(&self) -> String {
        match self {
            Self::OpenReview { shelf, .. } => format!("open review {shelf}"),
            Self::Vote { shelf, approve, .. } | Self::VoteOn { shelf, approve, .. } => {
                match approve {
                    Some(true) => format!("approve review {shelf}"),
                    Some(false) => format!("reject review {shelf}"),
                    None => format!("clear vote on review {shelf}"),
                }
            }
            Self::ReportStatus {
                shelf,
                version,
                context,
                state,
                ..
            } => format!("report {context} {state:?} for review {shelf} version {version}"),
            Self::CloseReview { shelf } => format!("close review {shelf}"),
            Self::SetReviewRules { .. } => "set review rules".to_owned(),
            Self::SetWebhook { name, url, .. } => format!("set webhook {name} to {url}"),
            Self::DeleteWebhook { name } => format!("delete webhook {name}"),
            Self::LinkIssues { cl, add, remove } => {
                format!(
                    "link change {cl} to [{}], unlink [{}]",
                    add.join(", "),
                    remove.join(", ")
                )
            }
            Self::RegisterPlugin {
                name,
                version,
                content,
                ..
            } => format!("register plugin {name}@{version} ({content})"),
            Self::DeletePlugin { name, version } => format!("delete plugin {name}@{version}"),
        }
    }
}
