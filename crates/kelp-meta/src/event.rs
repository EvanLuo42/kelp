//! Events derived from commands, as sent to webhooks.
//!
//! Commands that touch accounts, tokens, settings or webhooks themselves
//! produce no event, so secrets never leave the server this way.

use kelp_core::DepotPath;
use serde_json::{Value, json};

use crate::record::CheckState;
use crate::{Command, FileAction, FileChange, Governance};

/// What a command did, for webhooks.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// The kind, such as `submit` or `review.vote`.
    pub kind: &'static str,
    /// The files the event is about, for path filters.
    pub paths: Vec<DepotPath>,
    pub data: Value,
}

/// Returns the event of `cmd`, if it has one.
pub fn of(cmd: &Command) -> Option<Event> {
    let event = |kind, paths, data| Some(Event { kind, paths, data });
    match cmd {
        Command::Submit { cl, submission: s } => event(
            "submit",
            paths(&s.changes),
            json!({
                "cl": cl.0,
                "stream": s.stream.to_string(),
                "author": s.author,
                "workspace": s.workspace,
                "description": s.description,
                "files": files(&s.changes),
                "issues": s.issues,
                "fixes": s.fixes,
                "review": s.review.map(|r| r.0),
                "at": s.at,
            }),
        ),
        Command::Shelve { cl, contents: c } => event(
            "shelve",
            paths(&c.files),
            json!({
                "shelf": cl.0,
                "stream": c.stream.to_string(),
                "owner": c.owner,
                "description": c.description,
                "files": files(&c.files),
                "at": c.at,
            }),
        ),
        Command::DeleteShelf { cl } => event("shelf.delete", Vec::new(), json!({ "shelf": cl.0 })),
        Command::Governance(Governance::OpenReview {
            shelf,
            reviewers,
            by,
            at,
        }) => event(
            "review.open",
            Vec::new(),
            json!({ "shelf": shelf.0, "reviewers": reviewers, "by": by, "at": at }),
        ),
        Command::Governance(
            Governance::Vote {
                shelf,
                user,
                approve,
                at,
            }
            | Governance::VoteOn {
                shelf,
                user,
                approve,
                at,
                ..
            },
        ) => event(
            "review.vote",
            Vec::new(),
            json!({ "shelf": shelf.0, "user": user, "approve": approve, "at": at }),
        ),
        Command::Governance(Governance::ReportStatus {
            shelf,
            version,
            context,
            state,
            url,
            by,
            at,
        }) => event(
            "review.status",
            Vec::new(),
            json!({
                "shelf": shelf.0,
                "version": version,
                "context": context,
                "state": match state {
                    CheckState::Pending => "pending",
                    CheckState::Success => "success",
                    CheckState::Failure => "failure",
                },
                "url": url,
                "by": by,
                "at": at,
            }),
        ),
        Command::Governance(Governance::CloseReview { shelf }) => {
            event("review.close", Vec::new(), json!({ "shelf": shelf.0 }))
        }
        Command::AcquireLocks {
            holder,
            domain,
            paths,
        } => event(
            "lock",
            Vec::new(),
            json!({
                "user": holder.user,
                "workspace": holder.workspace,
                "domain": domain.0,
                "paths": paths,
            }),
        ),
        Command::ReleaseLocks {
            holder,
            domain,
            paths,
        } => event(
            "unlock",
            Vec::new(),
            json!({
                "user": holder.as_ref().map(|h| &h.user),
                "domain": domain.0,
                "paths": paths,
            }),
        ),
        Command::CreateDepot { name, .. } => {
            event("depot.create", Vec::new(), json!({ "name": name }))
        }
        Command::CreateStream {
            path, kind, parent, ..
        } => event(
            "stream.create",
            Vec::new(),
            json!({
                "stream": path.to_string(),
                "kind": format!("{kind:?}").to_lowercase(),
                "parent": parent.as_ref().map(ToString::to_string),
            }),
        ),
        Command::PublishArtifact { id, contents: c } => event(
            "artifact.publish",
            Vec::new(),
            json!({
                "id": id,
                "stream": c.stream.to_string(),
                "cl": c.cl.0,
                "name": c.name,
                "tags": c.tags,
                "by": c.by,
                "at": c.at,
            }),
        ),
        Command::Obliterate { paths } => event("obliterate", Vec::new(), json!({ "paths": paths })),
        Command::Governance(Governance::LinkIssues { cl, add, remove }) => event(
            "issue.link",
            Vec::new(),
            json!({ "cl": cl.0, "add": add, "remove": remove }),
        ),
        _ => None,
    }
}

fn paths(changes: &[FileChange]) -> Vec<DepotPath> {
    changes.iter().map(|c| c.path.clone()).collect()
}

fn files(changes: &[FileChange]) -> Value {
    changes
        .iter()
        .map(|c| {
            let (action, data) = match &c.action {
                FileAction::Add { data } => ("add", Some(data)),
                FileAction::Edit { data, .. } => ("edit", Some(data)),
                FileAction::Delete { .. } => ("delete", None),
            };
            json!({
                "path": c.path.to_string(),
                "action": action,
                "content": data.map(|d| d.content.to_string()),
                "size": data.map(|d| d.size),
            })
        })
        .collect()
}
