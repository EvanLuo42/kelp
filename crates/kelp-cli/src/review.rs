//! `kelp review`: the submit gate's reviews of shelves.

use crate::util::unexpected;
use std::path::PathBuf;

use clap::{Subcommand, ValueEnum};
use kelp_client::{Connection, ContentCache, plugins};
use kelp_core::DepotPath;
use kelp_core::typemap::Typemap;
use kelp_proto::rpc::{CheckState, ReviewInfo, ReviewState, ReviewSummary, SubmitAction};
use kelp_proto::{Reply, Request};

#[derive(Debug, Subcommand)]
pub enum ReviewCmd {
    /// Asks for a review of a shelf; shelving again adds a version.
    Open {
        shelf: u64,
        /// A user asked to review; may be repeated.
        #[arg(long = "reviewer")]
        reviewers: Vec<String>,
    },
    /// Lists reviews, newest first.
    List {
        /// Includes submitted and closed reviews.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        stream: Option<String>,
    },
    /// Shows a review's files, votes, statuses and requirements.
    Show { shelf: u64 },
    /// Approves the latest version.
    Approve {
        shelf: u64,
        /// The version you looked at; defaults to the latest, which is
        /// printed.
        #[arg(long)]
        version: Option<u32>,
    },
    /// Rejects the latest version, which blocks the submit.
    Reject {
        shelf: u64,
        #[arg(long)]
        version: Option<u32>,
    },
    /// Withdraws your vote.
    Unvote { shelf: u64 },
    /// Reports the outcome of an automated check, typically from CI.
    Status {
        shelf: u64,
        /// What was checked, such as `build`.
        context: String,
        #[arg(value_enum)]
        state: State,
        /// The version that was checked; defaults to the latest.
        #[arg(long)]
        version: Option<u32>,
        #[arg(long)]
        url: Option<String>,
    },
    /// Closes a review without submitting.
    Close { shelf: u64 },
    /// Prints the review rules table.
    Rules,
    /// Replaces the review rules table with a file (administrators only).
    SetRules { file: PathBuf },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum State {
    Pending,
    Success,
    Failure,
}

pub async fn run(
    conn: &Connection,
    cache: &ContentCache,
    command: ReviewCmd,
) -> anyhow::Result<()> {
    match command {
        ReviewCmd::Open { shelf, reviewers } => {
            conn.call(&Request::OpenReview { shelf, reviewers }).await?;
            println!("Opened review {shelf}.");
        }
        ReviewCmd::List { all, stream } => {
            let request = Request::ListReviews {
                stream,
                open_only: !all,
            };
            let Reply::Reviews(reviews) = conn.call(&request).await? else {
                return Err(unexpected());
            };
            for r in reviews {
                println!("{}", line(&r));
            }
        }
        ReviewCmd::Show { shelf } => {
            let Reply::Review(info) = conn.call(&Request::GetReview { shelf }).await? else {
                return Err(unexpected());
            };
            show(&info);
            describe(conn, cache, &info).await?;
        }
        ReviewCmd::Approve { shelf, version } => vote(conn, shelf, version, Some(true)).await?,
        ReviewCmd::Reject { shelf, version } => vote(conn, shelf, version, Some(false)).await?,
        ReviewCmd::Unvote { shelf } => vote(conn, shelf, None, None).await?,
        ReviewCmd::Status {
            shelf,
            context,
            state,
            version,
            url,
        } => {
            let version = match version {
                Some(v) => v,
                None => {
                    let Reply::Review(info) = conn.call(&Request::GetReview { shelf }).await?
                    else {
                        return Err(unexpected());
                    };
                    info.summary.version
                }
            };
            let state = match state {
                State::Pending => CheckState::Pending,
                State::Success => CheckState::Success,
                State::Failure => CheckState::Failure,
            };
            let request = Request::ReportStatus {
                shelf,
                version,
                context,
                state,
                url,
            };
            conn.call(&request).await?;
        }
        ReviewCmd::Close { shelf } => {
            conn.call(&Request::CloseReview { shelf }).await?;
        }
        ReviewCmd::Rules => {
            let Reply::Text(text) = conn.call(&Request::GetReviewRules).await? else {
                return Err(unexpected());
            };
            println!(
                "{}",
                text.as_deref().unwrap_or("# no review rules").trim_end()
            );
        }
        ReviewCmd::SetRules { file } => {
            let source = std::fs::read_to_string(file)?;
            conn.call(&Request::SetReviewRules { source }).await?;
        }
    }
    Ok(())
}

async fn vote(
    conn: &Connection,
    shelf: u64,
    version: Option<u32>,
    approve: Option<bool>,
) -> anyhow::Result<()> {
    let version = match version {
        Some(v) => v,
        None => {
            let Reply::Review(info) = conn.call(&Request::GetReview { shelf }).await? else {
                return Err(unexpected());
            };
            if approve.is_some() {
                eprintln!(
                    "Voting on version {} of review {shelf}.",
                    info.summary.version
                );
            }
            info.summary.version
        }
    };
    let request = Request::Vote {
        shelf,
        version,
        approve,
    };
    conn.call(&request).await?;
    Ok(())
}

fn line(r: &ReviewSummary) -> String {
    let state = match r.state {
        ReviewState::Open if r.ready => "ready".to_owned(),
        ReviewState::Open => "open".to_owned(),
        ReviewState::Submitted(cl) => format!("submitted as {cl}"),
        ReviewState::Closed => "closed".to_owned(),
    };
    let title = r.description.lines().next().unwrap_or_default();
    let author = r.author_name.as_deref().unwrap_or(&r.author);
    format!(
        "review {:<6} v{:<3} {author:<12} {:<16} {title}",
        r.shelf, r.version, state
    )
}

fn show(info: &ReviewInfo) {
    let r = &info.summary;
    println!("{}", line(r));
    println!("stream {}", r.stream);
    if !info.reviewers.is_empty() {
        println!("reviewers {}", info.reviewers.join(", "));
    }
    println!("\n{}\n", r.description.trim_end());
    for f in &info.files {
        let action = match f.action {
            SubmitAction::Add { .. } => "add",
            SubmitAction::Edit { .. } => "edit",
            SubmitAction::Delete { .. } => "delete",
        };
        println!("  {action:<6} {}", f.path);
    }
    if !info.checks.is_empty() {
        println!();
        for (requirement, met) in &info.checks {
            println!("  [{}] {requirement}", if *met { "x" } else { " " });
        }
    }
    for (user, version, approve) in &info.votes {
        let verdict = if *approve { "approved" } else { "rejected" };
        let stale = if *version == r.version {
            ""
        } else {
            " (older version)"
        };
        println!("{user} {verdict} v{version}{stale}");
    }
    for s in &info.statuses {
        let url = s
            .url
            .as_deref()
            .map(|u| format!("  {u}"))
            .unwrap_or_default();
        println!(
            "{} {:?} on v{} by {}{url}",
            s.context, s.state, s.version, s.by
        );
    }
}

/// Prints the summaries of the differ plugins the typemap assigns to the
/// review's files.
async fn describe(
    conn: &Connection,
    cache: &ContentCache,
    info: &ReviewInfo,
) -> anyhow::Result<()> {
    let depot = DepotPath::parse(&info.summary.stream)?.depot().to_owned();
    let Reply::Text(Some(source)) = conn.call(&Request::GetTypemap { depot }).await? else {
        return Ok(());
    };
    let typemap = Typemap::parse(&source)?;
    let mut printed = false;
    for f in &info.files {
        let Some(differ) = typemap.resolve(&f.path).differ else {
            continue;
        };
        let (old, new) = match f.action {
            SubmitAction::Add { file } => (None, Some(file.content)),
            SubmitAction::Edit { base, file } => (Some(base), Some(file.content)),
            SubmitAction::Delete { base } => (Some(base), None),
        };
        if !printed {
            println!();
            printed = true;
        }
        match plugins::describe(conn, cache, &differ, &f.path, old, new).await {
            Ok(text) => println!("  {text}"),
            Err(e) => println!("  {}: {differ} failed: {e}", f.path),
        }
    }
    Ok(())
}
