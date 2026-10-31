//! The state of the governance layer; see [`crate::governance`].

pub(in crate::db) mod issues;
pub(in crate::db) mod plugins;
pub(in crate::db) mod reviews;
pub mod webhooks;

use redb::WriteTransaction;

use super::apply::Origin;
use crate::{Governance, Result};

pub(super) fn apply(txn: &WriteTransaction, cmd: &Governance, origin: Origin) -> Result<()> {
    match cmd {
        Governance::OpenReview { .. }
        | Governance::Vote { .. }
        | Governance::VoteOn { .. }
        | Governance::ReportStatus { .. }
        | Governance::CloseReview { .. }
        | Governance::SetReviewRules { .. } => reviews::apply(txn, cmd),
        Governance::SetWebhook { .. } | Governance::DeleteWebhook { .. } => {
            webhooks::apply(txn, cmd)
        }
        Governance::LinkIssues { .. } => issues::apply(txn, cmd),
        Governance::RegisterPlugin { .. } | Governance::DeletePlugin { .. } => {
            plugins::apply(txn, cmd, origin)
        }
    }
}
