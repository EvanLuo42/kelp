use std::sync::Arc;

use kelp_core::ClNumber;
use kelp_meta::Governance;
use kelp_meta::acl::Level;
use kelp_meta::record::{self, Review};
use kelp_proto::rpc::{CheckState, ReviewInfo, ReviewState, ReviewSummary, StatusInfo};
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::change::to_proto;
use super::{Access, Response, blocking, meta_error, parse_path};
use crate::State;

pub(super) async fn open(
    state: &State,
    access: &Access,
    shelf: u64,
    reviewers: Vec<String>,
) -> Response {
    check_shelf(state, access, shelf)?;
    commit(
        state,
        access,
        Governance::OpenReview {
            shelf: ClNumber(shelf),
            reviewers,
            by: access.name().to_owned(),
            at: state.clock.now(),
        },
    )
    .await
}

pub(super) async fn vote(
    state: &State,
    access: &Access,
    shelf: u64,
    version: u32,
    approve: Option<bool>,
) -> Response {
    load(state, access, shelf)?;
    commit(
        state,
        access,
        Governance::VoteOn {
            shelf: ClNumber(shelf),
            user: access.name().to_owned(),
            version,
            approve,
            at: state.clock.now(),
        },
    )
    .await
}

pub(super) async fn report_status(
    state: &State,
    access: &Access,
    shelf: u64,
    version: u32,
    context: String,
    check: CheckState,
    url: Option<String>,
) -> Response {
    let review = load(state, access, shelf)?;
    for file in review.files() {
        access.require(Level::Write, &file.path)?;
    }
    let state_ = match check {
        CheckState::Pending => record::CheckState::Pending,
        CheckState::Success => record::CheckState::Success,
        CheckState::Failure => record::CheckState::Failure,
    };
    commit(
        state,
        access,
        Governance::ReportStatus {
            shelf: ClNumber(shelf),
            version,
            context,
            state: state_,
            url,
            by: access.name().to_owned(),
            at: state.clock.now(),
        },
    )
    .await
}

pub(super) async fn close(state: &State, access: &Access, shelf: u64) -> Response {
    let review = load(state, access, shelf)?;
    if !access.is_admin() && !review.author.eq_ignore_ascii_case(access.name()) {
        return Err(ErrorReply::new(
            ErrorKind::Forbidden,
            format!("review {shelf} belongs to {}", review.author),
        ));
    }
    commit(
        state,
        access,
        Governance::CloseReview {
            shelf: ClNumber(shelf),
        },
    )
    .await
}

pub(super) async fn get(state: &Arc<State>, access: Access, shelf: u64) -> Response {
    let state = state.clone();
    blocking(move || {
        let review = load(&state, &access, shelf)?;
        let reader = state.reader()?;
        let checks = reader.review_checks(&review).map_err(meta_error)?;
        Ok(Reply::Review(ReviewInfo {
            summary: summary(&review, checks.iter().all(|c| c.met)),
            reviewers: review.reviewers.clone(),
            files: review
                .files()
                .iter()
                .filter(|f| access.can(Level::Read, &f.path))
                .map(|f| to_proto(&review.stream, f))
                .collect(),
            votes: review
                .votes
                .iter()
                .map(|v| (v.user.clone(), v.version, v.approve))
                .collect(),
            statuses: review
                .statuses
                .iter()
                .map(|s| StatusInfo {
                    context: s.context.clone(),
                    version: s.version,
                    state: match s.state {
                        record::CheckState::Pending => CheckState::Pending,
                        record::CheckState::Success => CheckState::Success,
                        record::CheckState::Failure => CheckState::Failure,
                    },
                    url: s.url.clone(),
                    by: s.by.clone(),
                })
                .collect(),
            checks: checks.into_iter().map(|c| (c.requirement, c.met)).collect(),
        }))
    })
    .await
}

pub(super) async fn list(
    state: &Arc<State>,
    access: Access,
    stream: Option<String>,
    open_only: bool,
) -> Response {
    let stream = stream.as_deref().map(parse_path).transpose()?;
    let state = state.clone();
    blocking(move || {
        let reader = state.reader()?;
        let mut out = Vec::new();
        for review in reader
            .reviews(stream.as_ref(), open_only)
            .map_err(meta_error)?
        {
            if access.sees(&review.stream) {
                let checks = reader.review_checks(&review).map_err(meta_error)?;
                out.push(summary(&review, checks.iter().all(|c| c.met)));
            }
        }
        Ok(Reply::Reviews(out))
    })
    .await
}

pub(super) fn get_rules(state: &State) -> Response {
    let source = state.reader()?.review_rules_source().map_err(meta_error)?;
    Ok(Reply::Text(source))
}

pub(super) async fn set_rules(state: &State, access: &Access, source: String) -> Response {
    access.require_admin()?;
    commit(state, access, Governance::SetReviewRules { source }).await
}

/// Checks that the user can see shelf `shelf`.
fn check_shelf(state: &State, access: &Access, shelf: u64) -> Result<(), ErrorReply> {
    let record = state.reader()?.shelf(ClNumber(shelf)).map_err(meta_error)?;
    match record {
        Some(s) if access.sees(&s.contents.stream) => Ok(()),
        _ => Err(not_found("shelf", shelf)),
    }
}

/// Loads the review of `shelf` if the user can see it.
fn load(state: &State, access: &Access, shelf: u64) -> Result<Review, ErrorReply> {
    let review = state
        .reader()?
        .review(ClNumber(shelf))
        .map_err(meta_error)?;
    match review {
        Some(r) if access.sees(&r.stream) => Ok(r),
        _ => Err(not_found("review", shelf)),
    }
}

async fn commit(state: &State, access: &Access, cmd: Governance) -> Response {
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

fn summary(review: &Review, ready: bool) -> ReviewSummary {
    ReviewSummary {
        shelf: review.shelf.0,
        stream: review.stream.to_string(),
        author: review.author.clone(),
        description: review.description.clone(),
        version: review.version(),
        state: match review.state {
            record::ReviewState::Open => ReviewState::Open,
            record::ReviewState::Submitted(cl) => ReviewState::Submitted(cl.0),
            record::ReviewState::Closed => ReviewState::Closed,
        },
        ready,
    }
}

fn not_found(what: &str, shelf: u64) -> ErrorReply {
    super::not_found(format!("{what} {shelf}"))
}
