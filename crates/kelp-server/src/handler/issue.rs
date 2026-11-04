use std::sync::Arc;

use kelp_core::ClNumber;
use kelp_core::issue::parse_key;
use kelp_meta::Governance;
use kelp_proto::rpc::IssueDetail;
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::change::summary;
use super::{Access, Response, blocking, invalid, meta_error, not_found, parse_path};
use crate::State;

pub(super) async fn get(state: &Arc<State>, access: Access, key: String) -> Response {
    let key = parse_key(&key).ok_or_else(|| invalid(format!("{key:?} is not an issue key")))?;
    let state = state.clone();
    blocking(move || {
        let reader = state.reader()?;
        let changes = reader
            .issue_changes(&key)
            .map_err(meta_error)?
            .iter()
            .filter(|c| access.sees(&c.stream))
            .map(|c| summary(&reader, c))
            .collect::<Result<_, _>>()?;
        let reached = reader
            .issue_reach(&key)
            .map_err(meta_error)?
            .into_iter()
            .filter(|(s, _)| access.sees(s))
            .map(|(s, cl)| (s.to_string(), cl.0))
            .collect();
        Ok(Reply::Issue(IssueDetail {
            key,
            changes,
            reached,
        }))
    })
    .await
}

pub(super) async fn link(
    state: &State,
    access: &Access,
    cl: u64,
    add: Vec<String>,
    remove: Vec<String>,
) -> Response {
    let change = state
        .reader()?
        .changelist(ClNumber(cl))
        .map_err(meta_error)?
        .filter(|c| access.sees(&c.stream))
        .ok_or_else(|| not_found(format!("change {cl}")))?;
    if !access.administers(&change.stream) && !change.author.eq_ignore_ascii_case(access.name()) {
        return Err(ErrorReply::new(
            ErrorKind::Forbidden,
            format!("change {cl} belongs to {}", change.author),
        ));
    }
    let cmd = Governance::LinkIssues {
        cl: ClNumber(cl),
        add,
        remove,
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn issues_in(
    state: &Arc<State>,
    access: Access,
    stream: String,
    at: Option<u64>,
    since: Option<u64>,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let state = state.clone();
    blocking(move || {
        let reader = state.reader()?;
        let head = reader
            .stream(&stream)
            .map_err(meta_error)?
            .ok_or_else(|| not_found(format!("stream {stream}")))?
            .head;
        let found = reader
            .issues_in(&stream, at.map_or(head, ClNumber), since.map(ClNumber))
            .map_err(meta_error)?;
        Ok(Reply::IssueKeys(
            found
                .into_iter()
                .map(|(k, cls)| (k, cls.into_iter().map(|c| c.0).collect()))
                .collect(),
        ))
    })
    .await
}
